//! Repository operations on one project: snapshots, lines, staging, status and in-place updates.
use crate::db::{CommitResolution, PublishResult};
use crate::errors::{error, Kind as ErrorKind};

use crate::access::{Policy, Role, Rule, Who};
use crate::changes::{self, EntryChange};
use crate::ctx::Ctx;
use crate::db::need;
use crate::fsutil::{self, Src};
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Conflict, Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::status_cache::{FileFingerprint, StatusCache};
use crate::store::{check_name, Project};
use crate::tree::Tree;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub type Snap = crate::source::Source;

pub struct Repo<'a> {
    pub p: Project<'a>,
    /// The command that opened this repo. A restore of an evicted state connects with it.
    pub ctx: Option<&'a Ctx>,
}

/// An immutable source-state identity checked against this catalog's state kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub struct SourceStateId(layr_types::StateId);
impl SourceStateId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct AuxStateId(layr_types::StateId);
impl AuxStateId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone)]
#[must_use]
pub enum ResolvedRevision {
    Source(SourceStateId),
    Auxiliary(AuxStateId),
    View(crate::overlay::View),
}
impl ResolvedRevision {
    /// Explicitly project a view onto its shared state for commands that read a tree.
    pub fn into_state(self) -> String {
        match self {
            Self::Source(id) => id.as_str().to_owned(),
            Self::Auxiliary(id) => id.as_str().to_owned(),
            Self::View(view) => view.shared,
        }
    }
    /// Source-only operations retain the existing auxiliary-state refusal, including
    /// when an exact view projects onto its shared source history.
    pub fn into_source(self, repo: &Repo) -> Result<SourceStateId> {
        match self {
            Self::Source(id) => Ok(id),
            Self::Auxiliary(id) => repo.source_id(id.as_str()),
            Self::View(view) => repo.source_id(&view.shared),
        }
    }
}

/// A complete native folder, or a logical source without exporting its content.
pub fn snap(path: &Path) -> Snap {
    Snap::native(path)
}
pub fn same_tree(a: &Snap, b: &Snap) -> bool {
    a.same(b)
}

// Count complete and partial source checkouts, including native rebuilds, on the calling thread.
#[cfg(test)]
thread_local! {
    pub(crate) static MATERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A temporary tree retains its operation's deletion authority. Failed constructors never
/// turn an existing destination into cleanup authority.
pub struct Temp {
    pub path: PathBuf,
    resource: Option<TemporaryResource>,
}
enum TemporaryResource {
    Capture(crate::fs::OwnedCapture),
    Working(crate::fs::OwnedWorking),
    Existing(crate::fs::ResourceOwner),
    /// A folder outside the store's filesystem (a privileged check copy in /var/tmp): no custody,
    /// removed as a plain tree.
    Plain(PathBuf),
}
fn remove_plain(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("remove {}", path.display()))),
    }
}
impl TemporaryResource {
    fn abort(self, cause: anyhow::Error) -> anyhow::Error {
        match self {
            Self::Capture(captured) => captured.abort(cause),
            Self::Working(created) => created.abort(cause),
            Self::Existing(owner) => owner.abort(cause),
            Self::Plain(path) => match remove_plain(&path) {
                Ok(()) => cause,
                Err(e) => cause.context(format!("{e:#}")),
            },
        }
    }
    fn release(self) {
        match self {
            Self::Capture(captured) => captured.release(),
            Self::Working(created) => created.release(),
            Self::Existing(owner) => owner.release(),
            Self::Plain(_) => {}
        }
    }
}

impl Temp {
    pub(crate) fn abort(mut self, cause: anyhow::Error) -> anyhow::Error {
        match self.resource.take() {
            Some(resource) => resource.abort(cause),
            None => cause,
        }
    }
    pub(crate) fn discard(mut self) -> Result<()> {
        match self.resource.take() {
            Some(TemporaryResource::Capture(captured)) => captured.discard(),
            Some(TemporaryResource::Working(created)) => created.discard(),
            Some(TemporaryResource::Existing(owner)) => owner.discard(),
            Some(TemporaryResource::Plain(path)) => remove_plain(&path),
            None => Ok(()),
        }
    }
    /// A temporary folder that may be on any filesystem, removed as a plain tree.
    pub(crate) fn plain(path: PathBuf) -> Self {
        Self { path: path.clone(), resource: Some(TemporaryResource::Plain(path)) }
    }
    pub(crate) fn existing(path: PathBuf) -> Result<Self> {
        let owner = crate::fs::retain_owned(&path)?;
        Ok(Self { path, resource: Some(TemporaryResource::Existing(owner)) })
    }
    pub(crate) fn directory(path: PathBuf, mode: u32) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(mode).create(&path)?;
        Self::existing(path)
    }
    pub(crate) fn empty(path: PathBuf) -> Result<Self> {
        let owner = crate::fs::create_owned(&path)?;
        Ok(Self { path, resource: Some(TemporaryResource::Working(owner)) })
    }
    pub(crate) fn capture(
        src: &Path,
        path: PathBuf,
        protection: crate::fs::Protection,
        purpose: crate::fs::CapturePurpose,
    ) -> Result<Self> {
        let resource = match protection {
            crate::fs::Protection::Frozen => TemporaryResource::Capture(crate::fs::freeze_path_owned(src, &path, purpose)?),
            crate::fs::Protection::Writable => TemporaryResource::Working(crate::fs::copy_capture_owned(src, &path, purpose)?.0),
        };
        Ok(Self { path, resource: Some(resource) })
    }
    pub(crate) fn source(source: &Snap, path: PathBuf, protection: crate::fs::Protection) -> Result<Self> {
        let resource = match protection {
            crate::fs::Protection::Frozen => TemporaryResource::Capture(crate::fs::freeze_source(source, &path)?),
            crate::fs::Protection::Writable => {
                TemporaryResource::Working(crate::source::storage::create_stage(source, &path, None)?)
            }
        };
        Ok(Self { path, resource: Some(resource) })
    }
    pub(crate) fn selected(source: &Snap, paths: &[String], path: PathBuf) -> Result<Self> {
        let owner = crate::fs::export_selected_owned(source, paths, &path)?;
        Ok(Self { path, resource: Some(TemporaryResource::Working(owner)) })
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _trace = crate::trace::named(|| format!("discard {}", self.path.display()));
        let result = match self.resource.take() {
            Some(TemporaryResource::Capture(captured)) => captured.discard(),
            Some(TemporaryResource::Working(created)) => created.discard(),
            Some(TemporaryResource::Existing(owner)) => owner.discard(),
            Some(TemporaryResource::Plain(path)) => remove_plain(&path),
            None => Ok(()),
        };
        if let Err(error) = result {
            eprintln!("warning: {error:#}");
        }
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub staged: Vec<EntryChange>,
    pub unstaged: Vec<EntryChange>,
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
            s.insert(c.path().clone());
            if let Some(o) = c.old_path() {
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
        self.staged.retain(|c| !paths.contains(c.path()));
        self.unstaged.retain(|c| !paths.contains(c.path()));
        self.untracked.retain(|p| !paths.contains(p));
        self.conflicts.retain(|c| !paths.contains(&c.path));
    }
    pub fn clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
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
    before: Snap,
    after: Snap,
    own: Option<(u32, u32)>,
    view: Option<Temp>,
    cleanup: PublicationCleanup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PublicationCleanup {
    Rollback,
    Keep,
    Recover,
}
impl PublicationCleanup {
    pub fn from_failure(error: &crate::db::PublishError) -> Self {
        match error {
            CommitResolution::NotCommitted(_) => Self::Rollback,
            CommitResolution::Committed(_) => Self::Keep,
            CommitResolution::Unknown(_) => Self::Recover,
        }
    }
}

impl<'a> WorkingWrite<'a> {
    pub fn publication_failed(&mut self, error: &crate::db::PublishError) {
        self.cleanup = PublicationCleanup::from_failure(error);
    }

    /// `view` is the tree of an overlay view that `after` names (kept for the write back).
    pub fn new(
        repo: &'a Repo<'a>,
        wdir: PathBuf,
        own: Option<(u32, u32)>,
        before: Snap,
        after: Snap,
        view: Option<Temp>,
    ) -> Self {
        WorkingWrite { repo, wdir, before, after, own, view, cleanup: PublicationCleanup::Rollback }
    }

    /// Keep the write. Returns the overlay view that the write used, if any.
    pub fn commit(mut self) -> Option<Temp> {
        self.cleanup = PublicationCleanup::Keep;
        self.view.take()
    }
}

impl Drop for WorkingWrite<'_> {
    fn drop(&mut self) {
        if self.cleanup != PublicationCleanup::Rollback {
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

#[derive(Debug)]
pub struct TargetMoved {
    pub found: String,
}
impl std::fmt::Display for TargetMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("target head changed")
    }
}
impl std::error::Error for TargetMoved {}

/// The error of a command whose line head moved during the command. It names both heads.
/// `TargetMoved` lets the merge queue retry the entry once.
pub fn line_moved(name: &str, from: &str, to: &str) -> anyhow::Error {
    error(
        ErrorKind::Failed,
        format!(
            "error: line {name} moved during the command (head {} -> {}); run it again",
            crate::ids::short(from),
            crate::ids::short(to)
        ),
    )
    .context(TargetMoved { found: to.to_owned() })
}

/// The error of a command whose line was deleted during the command.
pub fn line_gone(name: &str) -> anyhow::Error {
    error(ErrorKind::Failed, format!("error: line {name} was deleted during the command; run it again"))
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
    /// A change of the working folder or the stage only, with the head in place (restore, clean,
    /// a reset to the head): the owner. Protection is about the history of the line, so a
    /// protected line allows it.
    Working,
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
        Self::from_project(ctx, Project::open(&ctx.store, project)?)
    }

    pub fn from_project(ctx: &'a Ctx, p: Project<'a>) -> Result<Repo<'a>> {
        let r = Repo { p, ctx: Some(ctx) };
        r.can_read(ctx)?;
        Ok(r)
    }

    // ----- permissions (docs/design.md, section 3; src/access.rs) -----

    pub fn policy(&self) -> Policy {
        Policy::load(&self.p)
    }

    pub fn who(ctx: &Ctx) -> Who {
        Who::caller(&ctx.caller)
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
                if reader_action && !self.p.is_overlay() && pol.denied_by(&who, action).is_none() && self.owns_a_line(ctx)? {
                    return Ok(());
                }
                Err(anyhow::anyhow!("permission denied: {reason}"))
            }
        }
    }

    /// The caller owns a line of this machine. A line whose home is another machine has an owner of that machine:
    /// the same number here can be another user, so it gives no right here.
    fn owns_a_line(&self, ctx: &Ctx) -> Result<bool> {
        let here = &self.p.store.machine.id;
        Ok(self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid && &l.machine == here))
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
        if !self.p.is_overlay() && pol.denied_by(&who, "read").is_none() && self.owns_a_line(ctx)? {
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
            LineOp::Save | LineOp::Working => {
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

    pub fn source_id(&self, id: &str) -> Result<SourceStateId> {
        Ok(SourceStateId(layr_types::StateId::parse(&self.source_state(id)?.id)?))
    }

    pub fn resolved_state(&self, id: &str) -> Result<ResolvedRevision> {
        let state = self.state(id)?;
        let id = layr_types::StateId::parse(&state.id)?;
        Ok(if state.kind.auxiliary() {
            ResolvedRevision::Auxiliary(AuxStateId(id))
        } else {
            ResolvedRevision::Source(SourceStateId(id))
        })
    }

    /// The complete tree of a state. On APFS a thin state gets a view (`thin::complete_tree`);
    /// commands that read few paths use `state_src` instead.
    pub fn state_root(&self, id: &str) -> Result<PathBuf> {
        self.check_state(id)?;
        crate::source::storage::complete(&self.p, id)
    }

    /// Every command that reads the tree of a state comes here (`state_root`, `state_src`). A
    /// state that retention evicted comes back from a store remote that holds it.
    fn check_state(&self, id: &str) -> Result<()> {
        if self.p.has_state(id) {
            return Ok(());
        }
        // A migrated state comes back from the object store with its exact times
        // (docs/objects.md, section 13). A freed state has no owner: it stays freed.
        if !matches!(self.p.db.state_flags(id)?, Some((_, true))) && crate::objstore::build_state(self, id)? {
            return Ok(());
        }
        // An imported state is built from git (docs/ownership.md, "States imported from Git"). A
        // copy on a store remote is the fallback when git cannot give the objects.
        if crate::gitbridge::buildable(self, id) {
            return crate::gitbridge::build_tree(self, id, None, None).or_else(|e| {
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
        if !matches!(self.p.db.state_flags(id)?, Some((_, true))) {
            if let Some((store, refs)) = crate::objstore::verified(&self.p, id)? {
                let tree = crate::object_tree::ObjectTree::new(store, refs, &self.p.meta_dir().join("object-reads"))?;
                return Ok(Snap::Objects(tree));
            }
        }
        self.check_state(id)?;
        Snap::of_state(&self.p, id)
    }

    /// `state_src` for a command that writes a working folder from the tree under its line lock
    /// only (`finish`): `gc` and `keyframe` take the project lock, so the layers must stay for the
    /// whole write and its write-back. A view that it reads is marked as used, as `state_root`
    /// marks it, so no `gc` removes it now. A chain longer than `thin.chain` can become a keyframe,
    /// which drops its top layer: it is read as a complete tree, as before.
    pub fn state_src_to_write(&self, id: &str) -> Result<Snap> {
        if crate::objstore::verified(&self.p, id)?.is_some() {
            return self.state_src(id);
        }
        self.check_state(id)?;
        if let Some(path) = crate::source::storage::prepare_object_read(&self.p, id)? {
            return Ok(snap(&path));
        }
        self.state_src(id)
    }

    /// The head of a line to read (`state_src`). A line with overlays reads its base.
    pub fn head_src(&self, line: &Line) -> Result<Snap> {
        self.head_src_with(line, false)
    }

    /// `head_src` with `state_src_to_write`.
    pub fn head_src_to_write(&self, line: &Line) -> Result<Snap> {
        self.head_src_with(line, true)
    }

    fn head_src_with(&self, line: &Line, write: bool) -> Result<Snap> {
        let layers = if self.p.is_overlay() { Vec::new() } else { self.layers(line)? };
        if layers.is_empty() {
            return if write { self.state_src_to_write(&line.head) } else { self.state_src(&line.head) };
        }
        let base = self.head_tree(line)?;
        Ok(snap(&base))
    }

    /// A complete folder of a tree. A layered tree gets a temporary read-only copy in `keep`.
    pub fn complete(&self, tree: Snap, keep: &mut Vec<Temp>) -> Result<PathBuf> {
        if let Some(path) = tree.folder() {
            return Ok(path.to_path_buf());
        }
        let path = self.p.scan_dir().join(crate::store::temp_name());
        #[cfg(test)]
        MATERIALIZATIONS.with(|count| count.set(count.get() + 1));
        let temporary = Temp::source(&tree, path, crate::fs::Protection::Frozen)?;
        let path = temporary.path.clone();
        keep.push(temporary);
        Ok(path)
    }
    /// Capture the same logical source on every native filesystem.
    pub fn snapshot_state(&self, id: &str, dst: &Path, readonly: bool) -> Result<()> {
        crate::fs::materialise_source(
            &self.state_src(id)?,
            dst,
            if readonly { crate::fs::Protection::Frozen } else { crate::fs::Protection::Writable },
        )
    }

    /// The working folder of a line: its recorded path (`layr line move`, or the folder that
    /// `line rename` kept), else `lines/<name>` unless another line keeps that folder
    /// (`Project::line_folder`).
    pub fn working(&self, line: &Line) -> PathBuf {
        self.p.line_folder(&line.id, &line.name)
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
        self.new_state_as(ctx, src, kind, parents, message, line, ids::new_id())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_state_as(
        &self,
        ctx: &Ctx,
        src: &Path,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
        id: String,
    ) -> Result<StateRec> {
        let dst = self.p.state_path(&id);
        let captured = ctx.captures.snapshot(src, &dst)?;
        match self.describe_state(ctx, id, kind, parents, message, line) {
            Ok(state) => {
                captured.release();
                Ok(state)
            }
            Err(error) => Err(captured.abort(error)),
        }
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
        let info = crate::fs::host().namespace.info(&dst)?;
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
            tree: None,
            times: None,
            cache_key_parts: Default::default(),
            parts: Default::default(),
        }
    }

    /// True when the tree of a state can be read now or built from git (`gitbridge::buildable`).
    pub fn tree_available(&self, id: &str) -> bool {
        self.p.has_state(id)
            || (!matches!(self.p.db.state_flags(id), Ok(Some((_, true))))
                && crate::objstore::verified(&self.p, id).is_ok_and(|r| r.is_some()))
            || crate::gitbridge::buildable(self, id)
    }

    /// An automatic state of a line's working content. For a line with overlays the snapshot
    /// leaves the overlay files out (a writable snapshot, the files removed, read-only): no state
    /// of the project holds a file of an overlay. The caller holds the project lock.
    pub fn working_snapshot(&self, ctx: &Ctx, line: &Line, parents: Vec<String>, message: &str) -> Result<StateRec> {
        self.working_snapshot_over(ctx, line, parents, message, &line.head)
    }

    /// The working content of `line` (folder `w`), whose folder shows the view of an overlay X, as an
    /// automatic state of X's own store. The records of X say which automatic state of the line it belongs
    /// to (`auto.<state>`), so no record of the project names it, and only readers of X read it (a reader of
    /// X reads every overlay below it). The caller holds the project lock.
    fn save_private_working(&self, ctx: &Ctx, line: &Line, w: &Path, state: &str, message: &str) -> Result<()> {
        let top = self.line_view_top(line)?.ok_or_else(|| anyhow::anyhow!("line '{}' shows no overlay", line.name))?;
        let orepo = self.overlay_repo(&top)?;
        let _lock = orepo.p.lock_maintenance()?;
        let st = orepo.new_state(ctx, w, StateKind::Auto, Vec::new(), message, None)?;
        let mut rec = crate::model::DraftRecord::new("overlay.use");
        rec.data = serde_json::json!({"config": {crate::overlay::auto_key(state): st.id}});
        rec.states.push(st.clone());
        if let Err(e) = orepo.commit_record(&_lock, ctx, rec) {
            if matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
                orepo.discard_states(std::slice::from_ref(&st));
            }
            return Err(e.into());
        }
        Ok(())
    }

    /// The tree to restore a working folder from for the automatic state `state` of `line`: the state
    /// that keeps its working content in the store of an overlay of the line's path (`save_private_working`),
    /// else the state itself. The second value says whether it holds the whole working content.
    pub fn working_source(&self, line: &Line, state: &str) -> Result<(PathBuf, bool)> {
        if !self.p.is_overlay() {
            for l in self.layers(line)?.into_iter().rev() {
                let Ok(op) = self.overlay_project(&l.overlay) else { continue };
                if let Some(id) = op.db.overlay_config(&crate::overlay::auto_key(state))?.and_then(|v| v.as_str().map(str::to_owned)) {
                    let orepo = Repo { p: op, ctx: self.ctx };
                    if let Ok(root) = orepo.state_root(&id) {
                        return Ok((root, true));
                    }
                    eprintln!(
                        "warning: the working changes that automatic state {} kept in overlay {} are gone; the folder gets the head of main for them",
                        ids::short(state),
                        l.overlay
                    );
                }
            }
        }
        Ok((self.state_root(state)?, false))
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
        self.working_snapshot_as(ctx, line, parents, message, base, ids::new_id())
    }

    #[allow(clippy::too_many_arguments)]
    fn working_snapshot_as(
        &self,
        ctx: &Ctx,
        line: &Line,
        parents: Vec<String>,
        message: &str,
        base: &str,
        id: String,
    ) -> Result<StateRec> {
        let mut resource = None;
        let mut metadata = None;
        let mut temporary = None;
        let made = (|| {
            #[cfg(target_os = "macos")]
            if let Some(view) = crate::views::of(self, line) {
                let state = crate::views::working_snapshot(ctx, self, line, &view, parents, message, base, id.clone())?;
                let path = if self.p.state_path(&id).is_dir() { self.p.state_path(&id) } else { self.p.delta_path(&id) };
                resource = Some(TemporaryResource::Existing(crate::fs::retain_owned(&path)?));
                return Ok(state);
            }
            let w = self.working(line);
            recover_cache_replacements(&w)?;
            self.restore_caches(line)?;
            // A line whose next named save sends changes into an overlay (`private_view`): its automatic
            // states hold the shared head, so no state that other readers of the project read holds a change
            // that may be proprietary (docs/overlays.md, section 9.4). Its changes have no automatic state here.
            if self.private_view(line)? {
                let head = self.state_root(&line.head)?;
                resource = Some(TemporaryResource::Capture(ctx.captures.snapshot(&head, &self.p.state_path(&id))?));
                let st = self.describe_state(ctx, id.clone(), StateKind::Auto, parents, message, Some(line))?;
                // The working content goes to the store of the overlay that the folder shows, which only its
                // readers read (docs/overlays.md section 1, automatic states): `undo` and the restore after a
                // refused command take it from there (`working_source`). A failure keeps the safe state.
                if let Err(e) = self.save_private_working(ctx, line, &w, &st.id, message) {
                    eprintln!("warning: the working changes of line {} have no automatic state: {e:#}", line.name);
                }
                return Ok(st);
            }
            let private = self.private_paths(line)?;
            // A build can keep a cache plain by holding it open for writing. Its physical
            // conversion is optional; its files still belong only to the cache overlay.
            let caches = self.plain_caches(line)?;
            if private.is_empty() && caches.is_empty() && self.layers(line)?.is_empty() {
                if let Some(format) = crate::source::storage::sparse_format(&self.p) {
                    let base = if self.p.has_state(base) { base } else { line.head.as_str() };
                    let source = self.state_src(base)?;
                    if let Some(captured) = format.write_snapshot(&self.p, &id, &source, base, &w)? {
                        resource = Some(TemporaryResource::Capture(captured));
                        metadata = crate::source::Source::of_state(&self.p, &id)?.layered_storage();
                        return self.describe_state(ctx, id.clone(), StateKind::Auto, parents, message, Some(line));
                    }
                }
            }
            if private.is_empty() && caches.is_empty() {
                resource = Some(TemporaryResource::Capture(ctx.captures.snapshot(&w, &self.p.state_path(&id))?));
                return self.describe_state(ctx, id.clone(), StateKind::Auto, parents, message, Some(line));
            }
            let copy = Temp::capture(
                &w,
                self.p.work_dir().join(crate::store::temp_name()),
                crate::fs::Protection::Writable,
                crate::fs::CapturePurpose::State,
            )?;
            let dst = copy.path.clone();
            temporary = Some(copy);

            let t = Tree::open(&dst)?;
            for p in private.iter().chain(&caches) {
                if t.stat(p).is_some() {
                    t.remove(p)?;
                }
            }
            drop(t);
            // An overlay file under another name leaves too: moved from an overlay path, or a
            // new file with the content of an overlay file (a copy, an editor's extra copy).
            let copies = if private.is_empty() { Vec::new() } else { self.private_copies(line, &dst, &private)? };
            if !copies.is_empty() {
                let t = Tree::open(&dst)?;
                for p in &copies {
                    t.remove(p)?;
                }
                drop(t);
            }
            let destination = self.p.state_path(&id);
            resource = Some(TemporaryResource::Capture(ctx.captures.snapshot(&dst, &destination)?));
            let info = crate::fs::host().namespace.info(&destination)?;
            Ok(StateRec {
                id: id.clone(),
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
                tree: None,
                times: None,
                cache_key_parts: Default::default(),
                parts: Default::default(),
            })
        })()
        .and_then(|state| {
            anyhow::ensure!(
                std::env::var_os("LAYR_TEST_WORKING_SNAPSHOT_ERROR").is_none(),
                "test: working snapshot failed after copying"
            );
            Ok(state)
        });
        match made {
            Ok(state) => {
                if let Some(resource) = resource {
                    resource.release();
                }
                Ok(state)
            }
            Err(error) => {
                if let Some(metadata) = metadata {
                    if let Err(cleanup) = metadata.forget_base(&self.p, &id) {
                        eprintln!("warning: {cleanup:#}");
                    }
                }
                Err(match resource {
                    Some(resource) => resource.abort(error),
                    None => error,
                })
            }
        }
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
            let Some(n) = c.new() else { continue };
            if c.old_path().map(|o| private.contains(o)).unwrap_or(false) {
                out.push(c.path().clone());
                continue;
            }
            if c.old().is_none() {
                for p in by_size.get(&n.size).into_iter().flatten() {
                    if fsutil::same_entry(&base, p, snap, c.path(), None)? {
                        out.push(c.path().clone());
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
            if crate::source::storage::recovery_retains(&self.p, &s.id) {
                // The journal owns rollback of the only staged copy.
                continue;
            }
            let _ = self.p.delete_state(&s.id);
        }
    }

    /// Read the authenticated records of the local database that have an effect here (`effective_records`): a
    /// record that the trust rules refuse is kept in the log but changes no view.
    pub fn read_model(&self) -> Result<layr_model::Project> {
        let records = self.p.authorized_records()?;
        layr_model::Project::from_authorized_records(&self.p.id, records, &self.p.store.machine.id)
    }

    /// Append a completed named transfer through the normal signed record path.
    pub fn record_transfer(&self, ctx: &Ctx, op: &str, remote: &str, url: &str, transfer: &crate::model::Transfer) -> Result<()> {
        let _lock = self.p.lock_maintenance()?;
        let mut rec = crate::model::DraftRecord::new(op);
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
        self.commit_record(&_lock, ctx, rec)?;
        Ok(())
    }

    /// Write a record. On failure, delete the states it names.
    pub fn commit_record(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        rec: crate::model::DraftRecord,
    ) -> PublishResult<Record> {
        let states = rec.states.clone();
        locked.check(&self.p)?;
        match locked.append(rec, &ctx.actor()) {
            Ok(r) => Ok(r),
            Err(e) => {
                if matches!(e, CommitResolution::NotCommitted(_)) {
                    discard_unrecorded(self, &states);
                }
                Err(e)
            }
        }
    }

    pub fn temp_snapshot(&self, src: &Path, readonly: bool) -> Result<Temp> {
        let dst = self.p.scan_dir().join(crate::store::temp_name());
        Temp::capture(
            src,
            dst,
            if readonly { crate::fs::Protection::Frozen } else { crate::fs::Protection::Writable },
            crate::fs::CapturePurpose::State,
        )
    }

    pub fn temp_work(&self, src: &Path) -> Result<Temp> {
        let dst = self.p.work_dir().join(crate::store::temp_name());
        Temp::capture(src, dst, crate::fs::Protection::Writable, crate::fs::CapturePurpose::State)
    }

    /// Read-only snapshot of a line's working content.
    pub fn scan(&self, line: &Line) -> Result<Temp> {
        let w = self.working(line);
        if !w.exists() {
            bail!("line '{}' has no working folder on this machine", line.name);
        }
        let dst = self.p.scan_dir().join(crate::store::temp_name());
        Temp::capture(&w, dst, crate::fs::Protection::Frozen, crate::fs::CapturePurpose::Scan)
    }

    /// `scan` limited to the paths that `specs` name (APFS): a restore of a few files then
    /// copies those files, not the whole working folder (seconds to minutes in a large line,
    /// under the store lock). A pattern or the whole folder takes the complete scan.
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
        Temp::selected(&Snap::native(&w), &paths.into_iter().collect::<Vec<_>>(), dst)
    }

    /// The newest automatic state of a line, if any.
    pub fn last_auto(&self, line: &Line) -> Result<Option<StateRec>> {
        // A state whose native copy was retired is in the object store: still the newest working content (an undo of a
        // restore went back to the head when the retirement came first, 2026-11-16).
        Ok(self.p.db.line_states(&line.id, StateKind::Auto)?.into_iter().find(|s| self.tree_available(&s.id)))
    }

    /// The state that a save compares the working content with: the newest automatic state made
    /// over the line's current head, else the head. An automatic state over an earlier head is
    /// older than the head and no base for "changed since".
    pub fn save_base(&self, line: &Line) -> Result<String> {
        let auto = self.last_auto(line)?.filter(|s| s.parents.contains(&line.head));
        Ok(auto.map(|s| s.id).unwrap_or_else(|| line.head.clone()))
    }

    /// Prepare deletion's final state before taking the project lock.
    pub fn prepare_final_state<'r>(&'r self, ctx: &Ctx, line: &Line) -> Result<PreparedAutomatic<'r, 'a>> {
        let prepared = self.prepare_save(ctx, line)?;
        if let Some(PreparedFolder { snapshot: AutomaticState::Snapshot { state, .. }, .. }) = &prepared.folder {
            crate::source::storage::complete(&self.p, &state.id)?;
        }
        Ok(prepared)
    }

    pub fn working_state(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
    ) -> Result<Option<String>> {
        locked.check(&self.p)?;
        if !self.working(line).exists() {
            return Ok(None);
        }
        Ok(match self.save(locked, ctx, line, message, false, false)? {
            Some(s) => Some(s.id),
            None => Some(self.save_base(line)?),
        })
    }

    pub fn prepare_working_caches(&self, line: &Line) -> Result<()> {
        self.restore_caches(line)?;
        self.prepare_cache_states(line)
    }

    /// Freeze and compare the working state while holding only the line lease.
    /// Dropping an unused preparation releases its unpublished native state.
    pub fn prepare_save<'r>(&'r self, ctx: &Ctx, line: &Line) -> Result<PreparedAutomatic<'r, 'a>> {
        #[cfg(target_os = "macos")]
        if crate::views::of(self, line).is_some() {
            return Ok(PreparedAutomatic { repo: self, folder: None });
        }
        let prev = self.save_base(line)?;
        self.hold(self.pin(&[&line.head, &prev])?);
        self.prepare_working_caches(line)?;
        let observed = self.observe(line, &self.select_index(line)?)?;
        let base = crate::source::storage::capture_base(&self.p, &line.head, &prev);
        let _prepare = crate::trace::span("prepare save");
        let id = ids::new_id();
        let pin = crate::free::prepare_state(&self.p, &id)?;
        let state = self.working_snapshot_as(ctx, line, vec![line.head.clone()], "", &base, id)?;
        let mut prepared = PreparedAutomatic {
            repo: self,
            folder: Some(PreparedFolder {
                observed,
                prev,
                snapshot: AutomaticState::Snapshot { state, comparison: None },
                _pin: pin,
            }),
        };
        if let Some(folder) = &mut prepared.folder {
            if self.tree_available(&folder.prev) {
                let base = self.state_src(&folder.prev)?;
                if let AutomaticState::Snapshot { state, comparison } = &mut folder.snapshot {
                    // In the view of an overlay the working content is in the overlay's store (`working_source`):
                    // the states of the project hold the shared head, so they compare equal.
                    if self.private_view(line)? {
                        let (old, old_full) = self.working_source(line, &folder.prev)?;
                        let (new, new_full) = self.working_source(line, &state.id)?;
                        if old_full == new_full && !changes::any_file_change(old.as_path(), new.as_path(), skip_path)? {
                            self.discard_states(std::slice::from_ref(state));
                            folder.snapshot = AutomaticState::Unchanged;
                        }
                        return Ok(prepared);
                    }
                    let compared = crate::objstore::compare(&base, self.state_src(&state.id)?)?;
                    if compared.files.iter().any(|c| !skip_path(c.path())) {
                        *comparison = Some(compared);
                    } else {
                        self.discard_states(std::slice::from_ref(state));
                        folder.snapshot = AutomaticState::Unchanged;
                    }
                }
            }
        }
        Ok(prepared)
    }

    /// Save the working content as an automatic state if it changed since the previous one.
    /// The caller holds the project lock.
    pub fn save(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        running: bool,
        force: bool,
    ) -> Result<Option<StateRec>> {
        locked.check(&self.p)?;
        #[cfg(target_os = "macos")]
        if let Some(view) = crate::views::of(self, line) {
            return crate::views::save_working(locked, ctx, self, line, &view, message);
        }
        let st = self.save_working(locked, ctx, line, message, running, force)?;
        // A cache folder that has no cache overlay state on this line yet gets its first one
        // (the cache of the line, as `save --overlays` makes it). A failure keeps the state.
        let of = match &st {
            Some(s) => s.id.clone(),
            None => self.save_base(line)?,
        };
        if let Err(e) = self.save_cache_states(locked, ctx, line, &of) {
            eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
        }
        clear_regenerable_memo();
        Ok(st)
    }

    fn save_working(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        running: bool,
        force: bool,
    ) -> Result<Option<StateRec>> {
        locked.check(&self.p)?;
        let _t = crate::trace::span("save working state");
        let w = self.working(line);
        if !w.exists() {
            return Ok(None);
        }
        recover_cache_replacements(&w)?;
        self.restore_caches(line)?;
        if !force
            && matches!(crate::fs::host().observations.read_policy(), crate::fs::observations::ReadPolicy::Validate)
            && self.private_paths(line)?.is_empty()
            && self.plain_caches(line)?.is_empty()
            // The view of an overlay keeps its working content in the overlay's store (`working_snapshot_as`):
            // a comparison with the folder here would decide on content that no state of the project holds.
            && !self.private_view(line)?
        {
            let prev = self.save_base(line)?;
            if self.tree_available(&prev) {
                let previous = self.state_src(&prev)?;
                if crate::fs::observations::validated_read(&[&w], None, || {
                    changes::any_file_change(&previous, &Snap::native(&w), skip_path)
                })? == Some(false)
                {
                    return Ok(None);
                }
            }
        }
        let base = crate::source::storage::capture_base(&self.p, &line.head, &self.save_base(line)?);
        let mut st = self.working_snapshot_over(ctx, line, vec![line.head.clone()], message, &base)?;
        st.running = running || crate::fsutil::take_unsettled();
        if !force {
            let checked = (|| -> Result<bool> {
                let prev = self.save_base(line)?;
                if !self.tree_available(&prev) {
                    return Ok(true);
                }
                // In the view of an overlay the working content is in the overlay's store (`working_source`):
                // the state itself holds the shared head, so compare the overlay's working content.
                let (old, old_full) = self.working_source(line, &prev)?;
                let (new, new_full) = self.working_source(line, &st.id)?;
                if old_full || new_full {
                    return Ok(old_full != new_full || changes::any_file_change(old.as_path(), new.as_path(), skip_path)?);
                }
                let base = self.state_src(&prev)?;
                let source = self.state_src(&st.id)?;
                if base.objects().is_some() && source.folder().is_some() && crate::objstore::enabled(&self.p)? {
                    let compared = crate::objstore::compare(&base, source)?;
                    let changed = compared.files.iter().any(|c| !skip_path(c.path()));
                    if changed {
                        compared.record(&self.p, &mut st)?;
                    }
                    Ok(changed)
                } else {
                    changes::any_file_change(&base, &source, skip_path)
                }
            })();
            let changed = match checked {
                Ok(checked) => checked,
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
        let mut rec = crate::model::DraftRecord::new("save");
        rec.states.push(st.clone());
        rec.data = serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": st.id});
        self.commit_record(locked, ctx, rec)?;
        Ok(Some(st))
    }

    /// The working content that the newest record of `line` left in the working folder: the state
    /// of an automatic save, the working state of a record, else the head. Undo of a save reverts
    /// its changes to this content. Reads at most 50 records, newest first.
    pub fn recorded_working(&self, line: &Line) -> Result<String> {
        for r in self.p.db.records_naming(&line.id, 50)? {
            if let Some(ch) = r.lines.get(&line.id) {
                let head = ch.after().map(|s| s.head.clone());
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

    /// The state that `save --overlays` gives a line's caches to: on a protected line with no
    /// changes since its head, the head, so the next lines made from that head start warm; else a
    /// new automatic state. Only a protected line gives caches to a state that other lines share,
    /// so such a state never holds a cache that only its maker may use. The caller holds the
    /// project lock. `prepared` (`prepare_caches_state`) is used when the head, the cache paths
    /// and the working folder are the ones it was made from; else the work is done here.
    pub fn caches_state(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        running: bool,
        caches: &[String],
        prepared: Option<PreparedCaches>,
    ) -> Result<StateRec> {
        locked.check(&self.p)?;
        let w = self.working(line);
        if !w.exists() {
            if let Some(p) = prepared {
                self.discard_states(std::slice::from_ref(&p.st));
            }
            return Err(error(ErrorKind::Failed, format!("error: the line '{}' has no working folder", line.name)));
        }
        let fresh = match prepared {
            Some(prepared)
                if prepared.head == line.head
                    && prepared.caches == caches
                    && crate::fs::observe_tree(&w)
                        .is_ok_and(|scan| scan.values().all(FileFingerprint::verifiable) && scan == prepared.scan) =>
            {
                Some(prepared)
            }
            Some(prepared) => {
                self.discard_states(std::slice::from_ref(&prepared.st));
                None
            }
            None => None,
        };
        let (mut st, same) = match fresh {
            Some(p) => (p.st, p.same),
            None => self.caches_snapshot(ctx, line, message, caches)?,
        };
        if same {
            self.discard_states(std::slice::from_ref(&st));
            return self.state(&line.head);
        }
        st.running = running;
        let mut rec = crate::model::DraftRecord::new("save");
        rec.states.push(st.clone());
        // The working state this record leaves: the next save of the line changes from it.
        rec.data = serde_json::json!({"line": line.id, "working_after": st.id});
        self.commit_record(locked, ctx, rec)?;
        Ok(st)
    }

    /// `save --overlays` before the project lock (issue #194): the snapshot of the working folder and
    /// its compare with the head, with the scan of the folder before them. The command holds the
    /// line lock. Off APFS a snapshot costs little under the lock: None.
    pub fn prepare_caches_state(
        &self,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        caches: &[String],
    ) -> Result<Option<PreparedCaches>> {
        if matches!(crate::fs::host().observations.read_policy(), crate::fs::observations::ReadPolicy::Capture) {
            return Ok(None);
        }
        let work = self.working(line);
        if !work.exists() {
            return Ok(None);
        }
        let _trace = crate::trace::span("prepare caches state");
        let scan = crate::fs::observe_tree(&work)?;
        if scan.values().any(|entry| !entry.verifiable()) {
            return Ok(None);
        }
        let (st, same) = self.caches_snapshot(ctx, line, message, caches)?;
        let current = crate::fs::observe_tree(&work);
        if !current.is_ok_and(|current| current == scan && current.values().all(FileFingerprint::verifiable)) {
            self.discard_states(std::slice::from_ref(&st));
            return Ok(None);
        }
        Ok(Some(PreparedCaches { head: line.head.clone(), caches: caches.to_vec(), scan, st, same }))
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
                let changed = changes::file_changes(&base, &made)?.into_iter().any(|c| {
                    !skip_path(c.path())
                        && !in_cache(c.path())
                        && (fsutil::lmeta(&base, c.src()).is_some() || !rules.ignored(c.path(), false))
                });
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
        let head = self.head_src(line)?;
        let base = (!self.p.is_overlay() && self.layers(line)?.is_empty()).then_some(line.head.as_str());
        let created = crate::source::storage::create_stage(&head, &p, base)?;
        // Keep the owner in the private index, so sealing a sparse save retains the
        // unchanged entries' ownership instead of making the next line copy them up.
        let (uid, gid) = self.owner_ids(line);
        give_tree(&p, uid, gid)?;
        crate::fsutil::atomic_write(&basef, key.as_bytes(), 0o600)?;
        created.release();
        Ok(p)
    }

    /// The staging line as a complete writable tree, for commands that change it in ways a
    /// thin layer does not support (a patch, a move, a removal, a restore).
    pub fn ensure_full_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.ensure_stage(line)?;
        let source = self.stage_tree(line, &p)?;
        if let Some(layered) = source.layered_storage() {
            let temporary = self.p.work_dir().join(crate::store::temp_name());
            let created = crate::fs::materialise_owned(&source, &temporary)?;
            let (uid, gid) = self.owner_ids(line);
            give_tree(&temporary, uid, gid)?;
            // The replacement is a complete native tree with a fresh marker. The physical
            // layer adapter swaps it atomically with the old index, then removes that layer.
            layered.publish_complete(created, &p)?;
        }
        Ok(p)
    }

    /// The staging namespace can be a complete tree or a layer over the same head source.
    pub fn stage_tree(&self, line: &Line, stage: &Path) -> Result<Snap> {
        Snap::stage(stage, self.head_src(line)?)
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
    pub fn index_source(&self, line: &Line) -> Result<(Snap, Option<PathBuf>)> {
        match self.current_stage(line)? {
            Some(s) => Ok((self.stage_tree(line, &s)?, Some(s))),
            None => Ok((self.head_src(line)?, None)),
        }
    }

    fn select_index(&self, line: &Line) -> Result<SelectedIndex> {
        // Bind the driver identity BEFORE reading complete/thin semantics. Validate
        // afterward too: a replacement cannot lend its token to the old semantics.
        let stage = self.current_stage(line)?.map(SelectedStage::capture).transpose()?;
        let source = match &stage {
            Some(stage) => self.stage_tree(line, &stage.path)?,
            None => self.head_src(line)?,
        };
        let selected = SelectedIndex { source, stage };
        selected.validate_stage()?;
        #[cfg(test)]
        if let Some(hook) = INDEX_SELECTED_HOOK.with_borrow_mut(Option::take) {
            hook();
        }
        Ok(selected)
    }

    /// Freeze exactly the selected index. In particular, a head selected without a stage
    /// stays that immutable head even if another command creates a stage meanwhile.
    fn snapshot_index(&self, selected: &SelectedIndex) -> Result<(Snap, Option<Temp>)> {
        selected.validate_stage()?;
        match &selected.stage {
            Some(stage) => {
                let temp = self.temp_snapshot(&stage.path, true)?;
                // Preserve the selected representation independently of native clone markers.
                let tree = match selected.source.below() {
                    Some(base) => base.attach(&temp.path)?,
                    None => snap(&temp.path),
                };
                selected.validate_stage()?;
                Ok((tree, Some(temp)))
            }
            None => Ok((selected.source.clone(), None)),
        }
    }

    /// The staging content as a read-only snapshot, or the head state when nothing is staged.
    pub fn index_view(&self, line: &Line) -> Result<(Snap, Option<Temp>)> {
        self.snapshot_index(&self.select_index(line)?)
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
        if let Some(paths) = limit {
            let mut status = Status::default();
            let list = changes::file_changes_of(index, scan, changes::candidates_selected(index, scan, paths)?)?;
            classify(scan, list, &mut status);
            self.finish_status(line, scan, index, false, &mut status)?;
            return Ok(status);
        }
        self.status_of_inner(line, scan, index, false)
    }

    fn status_of_inner(&self, line: &Line, scan: &Path, index: &Snap, full: bool) -> Result<Status> {
        self.status_of_with_policy(line, scan, index, full, None)
    }

    fn status_of_with_policy(&self, line: &Line, scan: &Path, index: &Snap, full: bool, local: Option<&[u8]>) -> Result<Status> {
        let _comparison = crate::trace::span("status full comparison");
        let mut st = Status::default();
        if full {
            let head = self.head_src(line)?;
            if !same_tree(index, &head) {
                let _t = crate::trace::span("status: head to index diff");
                // The index holds what `add` staged, an attribute-only change included.
                let diff = changes::file_changes(&head, index)?;
                let staged: Vec<EntryChange> = diff.into_iter().filter(|c| !skip_path(c.path())).collect();
                st.staged = changes::with_renames(&head, index, staged);
            }
        }
        let list = changes::file_changes(index, scan)?;
        match local {
            Some(local) => {
                let rules = Rules::captured(scan, list.iter().map(|c| c.path().as_str()), local)?;
                classify_with(&rules, list, &mut st);
            }
            None => classify(scan, list, &mut st),
        }
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
    pub fn index_token(&self, line: &Line) -> Result<IndexToken> {
        self.index_token_for(line, &self.select_index(line)?)
    }

    fn index_token_for(&self, line: &Line, selected: &SelectedIndex) -> Result<IndexToken> {
        selected.validate_stage()?;
        use layr_hashers::{Digest, GitCompatible};
        let mut h = GitCompatible::new();
        h.update(line.head.as_bytes());
        if let Some(stage) = &selected.stage {
            let started = crate::status_cache::clock()?;
            let scan = crate::fs::observe_tree(&stage.path)?;
            if !stage.root.trusted(stage.observed_before) || scan.values().any(|f| !f.trusted(started)) {
                // Unverifiable/racy index contents cannot validate a prepared comparison.
                h.update(ids::new_id().as_bytes());
            }
            h.update(&stage.identity);
            h.update(serde_json::to_vec(&scan)?);
        }
        selected.validate_stage()?;
        Ok(hex::encode(h.finalize()))
    }

    /// Capture immutable comparison inputs before taking the project lock.
    pub fn prepare_status(&self, line: &Line) -> Result<PreparedComparison> {
        let selected = self.select_index(line)?;
        let observed = self.observe(line, &selected)?;
        let (st, scan, index_temp, index) = self.status_selected(line, &selected)?;
        Ok(PreparedComparison { observed, st, capture: ComparisonCapture::Snapshot { scan, index_temp, index } })
    }

    pub fn uses_native_hints(&self, line: &Line) -> Result<bool> {
        Ok(crate::fs::prefers_captured_comparison(&self.select_index(line)?.source))
    }

    /// Commands that need only lists can reuse the verified status cache as well.
    pub fn prepare_status_report(&self, line: &Line) -> Result<PreparedComparison> {
        let selected = self.select_index(line)?;
        let observed = self.observe(line, &selected)?;
        let st = self.status_report_selected(line, &selected)?;
        Ok(PreparedComparison { observed, st, capture: ComparisonCapture::Report })
    }

    fn observe(&self, line: &Line, selected: &SelectedIndex) -> Result<ObservedVersion> {
        let started = crate::status_cache::clock()?;
        let work = self.working(line);
        Ok(ObservedVersion {
            line: line.id.clone(),
            head: line.head.clone(),
            index: self.index_token_for(line, selected)?,
            ignore_policy: Rules::new(&work).local_policy()?,
            started,
            entries: crate::fs::observe_tree(&work)?,
        })
    }

    /// Consume a validated capture; a changed input takes the ordinary snapshot path.
    pub fn locked_status(
        &self,
        locked: &crate::store::LockedProject<'_>,
        line: &Line,
        prepared: Option<PreparedComparison>,
    ) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        if let Some(prepared) = prepared {
            if let Some(validated) = prepared.revalidate(self, locked)? {
                let PreparedComparison { st, capture, .. } = validated.prepared;
                if let ComparisonCapture::Snapshot { scan, index_temp, index } = capture {
                    return Ok((st, scan, index_temp, index));
                }
            }
        }
        self.status(line)
    }

    pub fn locked_status_report(
        &self,
        locked: &crate::store::LockedProject<'_>,
        line: &Line,
        prepared: Option<PreparedComparison>,
    ) -> Result<Status> {
        if let Some(prepared) = prepared {
            if let Some(validated) = prepared.revalidate(self, locked)? {
                return Ok(validated.prepared.st);
            }
        }
        self.status_report(line)
    }

    pub(crate) fn prepare_capture(&self, source: &Path) -> Result<()> {
        if let Some(ctx) = self.ctx {
            ctx.captures.prepare(source, &self.p.work_dir())?;
        }
        Ok(())
    }

    /// Discard captures this command did not publish, after releasing the project lock.
    pub fn discard_prepared(&self) {
        if let Some(ctx) = self.ctx {
            if let Err(error) = ctx.captures.discard() {
                eprintln!("warning: {error:#}");
            }
        }
    }

    pub fn status(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        self.status_selected(line, &self.select_index(line)?)
    }

    fn status_selected(&self, line: &Line, selected: &SelectedIndex) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        let scan = self.scan(line)?;
        let (index, itemp) = self.snapshot_index(selected)?;
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

    /// Report from the live folder, validating every content read before caching it.
    /// Commands that need retained bytes still use `status`.
    pub fn status_report(&self, line: &Line) -> Result<Status> {
        #[cfg(target_os = "macos")]
        if let Some(view) = crate::views::of(self, line) {
            return crate::views::working_status(self, line, &view);
        }
        self.status_report_selected(line, &self.select_index(line)?)
    }

    fn status_report_selected(&self, line: &Line, selected: &SelectedIndex) -> Result<Status> {
        selected.validate_stage()?;
        let work = self.working(line);
        if std::env::var_os("LAYR_NO_STATUS_CACHE").is_some() {
            return Ok(self.status_selected(line, selected)?.0);
        }
        let started = crate::status_cache::clock()?;
        let local = match Rules::new(&work).local_policy() {
            Ok(local) => local,
            Err(_) => return Ok(self.status_selected(line, selected)?.0),
        };
        // Native hints compare a captured tree with this known native base. Avoid an eager
        // whole-tree stat scan merely to validate the status cache before that comparison.
        // Object-only bases have no native relationship and retain stat-cache validation.
        if crate::fs::prefers_captured_comparison(&selected.source) {
            let scan = self.scan(line)?;
            let (index, _itemp) = self.snapshot_index(selected)?;
            return self.status_of_with_policy(line, &scan.path, &index, true, Some(&local));
        }
        let Some(key) = self.status_key(line, &work, selected, &local).ok().flatten() else {
            return Ok(self.status_selected(line, selected)?.0);
        };
        let now = match crate::fs::observe_tree(&work) {
            Ok(scan) => scan,
            Err(_) => return Ok(self.status_selected(line, selected)?.0),
        };
        match self.cached_status(line, &work, selected, &key, &now, started, &local) {
            Ok(Some(st)) => return Ok(st),
            Ok(None) => {}
            Err(e) => eprintln!("warning: saved status failed, comparing the whole folder: {e:#}"),
        }
        // Keep the snapshot fallback for unreadable, mapped, or concurrently written entries.
        // The baseline precedes the snapshot: a change during it invalidates the next read.
        let scan = self.scan(line)?;
        let (frozen_index, _itemp) = self.snapshot_index(selected)?;
        let st = match self.status_of_with_policy(line, &scan.path, &frozen_index, true, Some(&local)) {
            Ok(st) => st,
            Err(_) => return Ok(self.status_selected(line, selected)?.0),
        };
        selected.validate_stage()?;
        let current_stage = self.current_stage(line)?;
        if current_stage.as_deref() == selected.stage.as_ref().map(|stage| stage.path.as_path())
            && Rules::new(&work).local_policy().is_ok_and(|current| current == local)
            && self.status_key(line, &work, selected, &local).ok().flatten().as_ref() == Some(&key)
            && crate::fs::observe_tree(&work).is_ok_and(|again| again == now)
        {
            let _ = self.save_status_cache(line, &StatusCache::new(key, started, now, st.clone()));
        }
        Ok(st)
    }

    pub fn prepare_left_out(&self, line: &Line, index: &Snap) -> Result<LeftOutComparison> {
        let work = self.working(line);
        let started = crate::status_cache::clock()?;
        let entries = crate::fs::observe_tree(&work)?;
        let local = Rules::new(&work).local_policy()?;
        let scan = self.scan(line)?;
        let st = self.status_of_with_policy(line, &scan.path, index, false, Some(&local))?;
        Ok(LeftOutComparison { started, entries, local, st })
    }

    pub fn cache_left_out(&self, line: &Line, comparison: LeftOutComparison) -> Result<()> {
        let selected = self.select_index(line)?;
        let work = self.working(line);
        if Rules::new(&work).local_policy()? != comparison.local {
            return Ok(());
        }
        if let Some(key) = self.status_key(line, &work, &selected, &comparison.local)? {
            self.save_status_cache(line, &StatusCache::new(key, comparison.started, comparison.entries, comparison.st))?;
        }
        Ok(())
    }

    fn status_cache_path(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("status").join(format!("{}.cache", line.id))
    }

    fn save_status_cache(&self, line: &Line, cache: &StatusCache) -> Result<()> {
        // A lost cache costs only time: no fsync, but never a partial file.
        let path = self.status_cache_path(line);
        let dir = path.parent().context("status cache folder")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.tmp", ids::new_id()));
        let result = (|| -> Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            file.write_all(&cache.encode()?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Immutable base tree, mutable index, working identity, version and ignore policy.
    fn status_key(&self, line: &Line, work: &Path, selected: &SelectedIndex, local: &[u8]) -> Result<Option<String>> {
        selected.validate_stage()?;
        use layr_hashers::{Digest, GitCompatible};
        if self.p.is_overlay() || !self.layers(line)?.is_empty() {
            return Ok(None);
        }
        let mut h = GitCompatible::new();
        let w = std::fs::symlink_metadata(work)?;
        h.update(format!("{}|{}|{}|{}|{}|", env!("CARGO_PKG_VERSION"), line.head, w.dev(), w.ino(), work.display()));
        if let Some(objects) = selected.source.objects() {
            h.update(objects.refs.tree.0);
        }
        // Local rules may live outside a snapshot (or a nested .git subvolume).
        h.update(local);
        if let Some(stage) = &selected.stage {
            if !stage.root.trusted(stage.observed_before) {
                return Ok(None);
            }
            h.update(std::fs::read(self.stage_base_file(line))?);
            h.update(&stage.identity);
            let started = crate::status_cache::clock()?;
            let scan = crate::fs::observe_tree(&stage.path)?;
            if scan.values().any(|f| !f.trusted(started)) {
                return Ok(None);
            }
            h.update(serde_json::to_vec(&scan)?);
        }
        selected.validate_stage()?;
        Ok(Some(hex::encode(h.finalize())))
    }

    fn cached_status(
        &self,
        line: &Line,
        work: &Path,
        selected: &SelectedIndex,
        key: &str,
        now: &BTreeMap<String, FileFingerprint>,
        started: u64,
        local: &[u8],
    ) -> Result<Option<Status>> {
        let index = &selected.source;
        let Ok(raw) = std::fs::read(self.status_cache_path(line)) else { return Ok(None) };
        let Some(cache) = StatusCache::decode(&raw, key) else { return Ok(None) };
        let _t = crate::trace::span("saved status");
        let paths = cache.changed(now);
        if paths.len() > 20_000 || paths.iter().any(|p| matches!(p.rsplit('/').next(), Some(".gitignore" | ".layrignore"))) {
            return Ok(None);
        }
        if paths.iter().any(|p| now.get(p).is_some_and(|f| !f.verifiable())) {
            return Ok(None);
        }
        // Every child has its own fingerprint, including children of deleted directories.
        // A directory's changed timestamps must not force hashing its unchanged children.
        let list = paths
            .iter()
            .filter_map(|p| {
                changes::EntryChange::observed(
                    p.clone(),
                    fsutil::lmeta(index, p),
                    fsutil::lmeta(work, p),
                    crate::compare::DataEvidence::Unknown,
                )
            })
            .collect();
        let list = changes::file_changes_of(index, work, list)?;
        let mut st = cache.status;
        st.unstaged.retain(|c| !paths.contains(c.path()) && !c.old_path().is_some_and(|p| paths.contains(p)));
        st.untracked.retain(|p| !paths.contains(p));
        st.ignored.retain(|p| !paths.contains(p));
        let rules = Rules::captured(work, paths.iter().map(String::as_str), local)?;
        classify_with(&rules, list, &mut st);
        st.unstaged.sort_by(|a, b| a.path().cmp(b.path()));
        st.untracked.sort();
        st.ignored.sort();
        self.finish_status(line, work, index, true, &mut st)?;
        if !rules.captured_matches()? {
            return Ok(None);
        }
        // Re-scan the changed paths, and check the mutable index and local rules too.
        // Unverifiable entries always take a snapshot rather than trusting stat equality.
        if crate::fs::observe_paths(work, &paths)?
            != now.iter().filter(|(p, _)| paths.contains(*p)).map(|(p, f)| (p.clone(), f.clone())).collect()
        {
            return Ok(None);
        }
        selected.validate_stage()?;
        let current_stage = self.current_stage(line)?;
        if current_stage.as_deref() != selected.stage.as_ref().map(|stage| stage.path.as_path())
            || self.status_key(line, work, selected, local)?.as_deref() != Some(key)
        {
            return Ok(None);
        }
        if !paths.is_empty() {
            let _ = self.save_status_cache(line, &StatusCache::new(key.to_string(), started, now.clone(), st.clone()));
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
        from: &(impl fsutil::Src + ?Sized),
        to: &(impl fsutil::Src + ?Sized),
        target: &Path,
        own: Option<(u32, u32)>,
        local: &BTreeSet<String>,
        force: bool,
    ) -> Result<Vec<EntryChange>> {
        let _t = crate::trace::span("write working folder");
        let list: Vec<EntryChange> = changes::file_changes(from, to)?.into_iter().filter(|c| !skip_path(c.path())).collect();
        if !force {
            let mut hit = BTreeSet::new();
            for c in &list {
                for p in [Some(c.path().as_str()), c.old_path().map(String::as_str)].into_iter().flatten() {
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
                return Err(error(ErrorKind::Failed,
                    format!(
                        "error: Your local changes to the following files would be overwritten:\n{}\nPlease save your changes or park them.",
                        names.join("\n")
                    ),
                ));
            }
        }
        // Moves keep the inode; then deletes (a folder can replace a file); then writes.
        for c in &list {
            if let Some(src) = c.old_path() {
                let t = Tree::open(target)?;
                if t.stat(src).is_some() && t.stat(c.path()).is_none() {
                    t.mkdirs_for(c.path(), own)?;
                    t.rename(src, c.path())?;
                } else {
                    fsutil::remove_entry(target, src)?;
                }
                prune_empty_parents(target, to, src)?;
            }
        }
        for c in &list {
            if c.old_path().is_some() {
                continue;
            } else if c.new().is_none() {
                fsutil::remove_entry(target, c.path())?;
                prune_empty_parents(target, to, c.path())?;
            }
        }
        for c in &list {
            if c.new().is_some() {
                fsutil::copy_work_entry(to, c.path(), target, c.path(), own)?;
            }
        }
        Ok(list)
    }

    // ----- lines -----

    /// Create a line from a state: a writable snapshot at `lines/<name>`.
    pub fn create_line(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        name: &str,
        start: &str,
        owner: u32,
    ) -> Result<Line> {
        locked.check(&self.p)?;
        self.create_line_from(locked, ctx, name, start, owner, None)
    }

    /// Create a line; a line made from another line (`source`) shows the overlays of the source
    /// that its owner reads (docs/overlays.md).
    pub fn create_line_from(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        name: &str,
        start: &str,
        owner: u32,
        source: Option<&Line>,
    ) -> Result<Line> {
        locked.check(&self.p)?;
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
        // A line that layr makes itself (the first line of a project, the line of a check) shows `main`: a
        // check of a merge into `main` runs on the view of `main` (docs/overlays.md section 1), and its build
        // output never holds files of an overlay.
        let _ = source;
        let layers: Vec<crate::overlay::Layer> = Vec::new();
        self.materialize_with(&line, start, &dst, &layers, true)?;
        let res = (|| -> PublishResult<Record> {
            // Not pruned: this path runs under the project lock, and only `init` uses it, which takes no caches.
            self.take_caches(ctx, &line, start, source, &dst)?;
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
            if !layers.is_empty() {
                if !self.public_layers(&layers)?.is_empty() {
                    rec.data["folder_view"] = self.public_view_data(&line.id, &layers)?;
                }
            }
            locked.append(rec, &ctx.actor())
        })();
        if let Err(e) = res {
            if matches!(e, CommitResolution::NotCommitted(_)) {
                let _ = crate::fs::remove(&dst);
                let _ = self.prune_bases(&line, None, 0);
            }
            return Err(e.into());
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
            let _ = crate::fs::remove(&tmp);
        }
        r
    }

    /// `cached`: folders that the caller already took from a cache (`take_caches`); they are not
    /// copied again from a warm cache.
    pub(crate) fn prepare_at(&self, line: &Line, state: &str, w: &Path, cached: &[String]) -> Result<()> {
        let (uid, gid) = self.owner_ids(line);
        // A matching root does not prove that inherited entries have matching owners.
        // Walk every private copy, but do not chown matching entries: on OverlayFS even
        // an ownership-only change copies a lower file into the writable upper.
        give_tree(w, uid, gid)?;
        #[cfg(target_os = "macos")]
        crate::sys::apfs_clear_acl(w)?;
        std::fs::set_permissions(w, std::fs::Permissions::from_mode(0o700))?;
        crate::gitbridge::make_git_folder(self, line, state, w)?;
        let folders: Vec<String> = self.cache_candidates(w)?.into_iter().filter(|folder| !cached.contains(folder)).collect();
        let mut building = line.clone();
        building.head = state.to_string();
        self.relocate_folders(&building, &folders, w, &self.working(line), crate::cache_overlay::CacheAccess::Prepared)
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
                head = Some(self.state_src(&line.head)?);
                stage = Some(self.current_stage(line)?);
            }
            // A file that the head or the stage (`add -f`) versions makes the folder no cache.
            let versioned = holds_files_in(head.as_ref().unwrap(), &p)
                || stage.as_ref().unwrap().as_ref().is_some_and(|s| holds_files(s, &p));
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
                    crate::busy::writer_of(&path)?.map(|u| format!("in use by {u}"))
                };
                out.push(Leftover { line: line.name.clone(), rel, bytes: tree_bytes(&path), path, kept });
            }
        }
        Ok(out)
    }

    /// Make the cache paths that a tool made again as plain folders caches again, before a
    /// snapshot of the working folder. The caller holds the project lock.
    pub fn restore_caches(&self, line: &Line) -> Result<()> {
        self.prepare_cache_locations(line)?;
        let (w, (u, g)) = (self.working(line), self.owner_ids(line));
        for p in self.plain_caches(line)? {
            if let Some(register) = crate::fs::host().register_cache {
                register(&fsutil::safe_join(&w, &p)?)?;
            } else if let Err(error) = convert_cache(&w, &p, u, g) {
                if error.downcast_ref::<CacheRefused>().is_none() {
                    return Err(error);
                }
                let _trace = crate::trace::span(format!("cache {p} not converted: {error}"));
            }
        }
        Ok(())
    }

    /// The cache folders of a line that are layers now but have no cache overlay state on this
    /// line yet. `save_cache_states` makes that first state, so a new line from a state of this
    /// line starts warm, without `save --overlays`.
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
            if !crate::fs::host().namespace.managed(&cp) {
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
                None => crate::cache_overlay::cache_overlay_name(&p, &crate::model::cache_platform()),
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
        Ok(crate::overlay::find_overlay_line(&orepo.p.db, &info, line)?.is_some_and(|ol| ol.head != info.root))
    }

    /// Before the lock (APFS): the copy that the first cache state of each unsaved cache takes
    /// under the lock with one rename (`save_cache_overlays`), as `save --overlays` makes it.
    pub fn prepare_cache_states(&self, line: &Line) -> Result<()> {
        self.prepare_cache_locations(line)?;
        let w = self.working(line);
        for p in self.unsaved_cache_layers(line)? {
            self.prepare_capture(&fsutil::safe_join(&w, &p)?)?;
        }
        Ok(())
    }

    /// The first cache state of each cache folder of `line` that has none (`unsaved_cache_layers`),
    /// saved with the automatic state `of`. The caller holds the project lock.
    fn save_cache_states(&self, locked: &crate::store::LockedProject<'_>, ctx: &Ctx, line: &Line, of: &str) -> Result<()> {
        locked.check(&self.p)?;
        let paths = self.unsaved_cache_layers(line)?;
        if paths.is_empty() {
            return Ok(());
        }
        let shared = self.policy().rule_for(&line.name).is_some();
        let made = self.save_cache_overlays(locked, ctx, line, &paths, shared, of);
        // Copies that no snapshot took (prepared before the lock) go to the trash.
        self.discard_prepared();
        made.map(|_| ())
    }

    /// The build-output overlays of `line` for a named save (docs/overlays.md, section 1a, slice
    /// 6): each cache path whose folder is a layer and holds no versioned files, with its overlay (if
    /// it has one yet) and whether the save makes a new part. A new part is due when the line has
    /// no part on the overlay yet, or when a file or folder in it changed since the line's part
    /// (`folder_changed_since`). An unchanged folder keeps its part, so a named save of a line with
    /// big build output does not copy it again.
    fn named_cache_overlays(&self, line: &Line) -> Result<Vec<(String, Option<String>, bool)>> {
        let w = self.working(line);
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut head = None;
        let mut stage = None;
        let mut out = Vec::new();
        for p in self.cache_paths(&w)? {
            let cp = fsutil::safe_join(&w, &p)?;
            if !crate::fs::host().namespace.managed(&cp) {
                continue;
            }
            if head.is_none() {
                head = Some(self.state_root(&line.head)?);
                stage = Some(self.current_stage(line)?);
            }
            if holds_files(head.as_ref().unwrap(), &p) || stage.as_ref().unwrap().as_ref().is_some_and(|s| holds_files(s, &p)) {
                continue;
            }
            let Some(name) = self.folder_overlay_of(&p)? else {
                out.push((p, None, true));
                continue;
            };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let current = crate::overlay::find_overlay_line(&orepo.p.db, &info, line)?.map(|ol| ol.head);
            let due = match current.as_deref() {
                Some(id) if id != info.root && orepo.p.has_state(id) => {
                    let since = orepo.p.db.state(id)?.map(|st| st.time).unwrap_or(0);
                    folder_changed_since(&cp, since)
                }
                _ => true,
            };
            out.push((p, Some(name), due));
        }
        Ok(out)
    }

    /// Before the lock (APFS): the copy of each build-output folder whose part a named save makes
    /// (`named_cache_overlays`), taken under the lock with one rename (`save_cache_overlays`).
    pub fn prepare_named_cache_parts(&self, line: &Line) -> Result<()> {
        self.prepare_cache_locations(line)?;
        let w = self.working(line);
        for (p, _, due) in self.named_cache_overlays(line)? {
            if due {
                self.prepare_capture(&fsutil::safe_join(&w, &p)?)?;
            }
        }
        Ok(())
    }

    /// A named save of `line` that makes the state `of`: the part of each build-output overlay of the
    /// line (docs/overlays.md, section 1a, slice 6). A changed folder gets a new part, saved with
    /// `of`; the overlay is made for this platform when it is missing (`regenerable`). An unchanged
    /// folder names the part that the line has. Returns the parts for the state record. A failure
    /// is a warning: the named save goes on without that part. The caller holds the project lock.
    pub fn save_named_cache_parts(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        of: &str,
    ) -> BTreeMap<String, String> {
        let mut parts = BTreeMap::new();
        let r = (|| -> Result<()> {
            let overlays = self.named_cache_overlays(line)?;
            let due: Vec<String> = overlays.iter().filter(|(_, _, due)| *due).map(|(p, _, _)| p.clone()).collect();
            for (_, name, due) in &overlays {
                let Some(name) = name.as_ref().filter(|_| !due) else { continue };
                let info = self.need_overlay(name)?;
                let orepo = self.overlay_repo(name)?;
                if let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, line)? {
                    parts.insert(name.clone(), ol.head);
                }
            }
            if !due.is_empty() {
                let shared = self.policy().rule_for(&line.name).is_some();
                let made = self.save_cache_overlays(locked, ctx, line, &due, shared, of);
                self.discard_prepared();
                for (_, name, id) in made? {
                    parts.insert(name, id);
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("warning: build-output overlay of line {} not saved: {e:#}", line.name);
        }
        parts
    }

    /// The cache paths of a line (`cache_paths`) in two lists: the caches, and the paths whose
    /// folder holds files in the head of the line. Such a folder is versioned: never a cache.
    pub fn line_cache_paths(&self, line: &Line) -> Result<(Vec<String>, Vec<String>)> {
        // The layers of the head: a complete view of a thin head cost one clone of the whole tree
        // after every merge (its `save --overlays`).
        let head = self.state_src(&line.head)?;
        Ok(self.cache_paths(&self.working(line))?.into_iter().partition(|p| !holds_files_in(&head, p)))
    }

    /// Record a new head for a line (and optionally a working state change).
    #[allow(clippy::too_many_arguments)]
    /// Move the head of `line` to `new_head` and record it, under the project lock.
    pub fn move_head(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        op: &str,
        line: &Line,
        new_head: &str,
        states: Vec<StateRec>,
        working_before: Option<String>,
        data: serde_json::Value,
    ) -> PublishResult<Record> {
        locked.check(&self.p)?;
        let rec = self.head_record(ctx, op, line, new_head, states, working_before, data)?;
        let r = self.commit_record(locked, ctx, rec)?;
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
    ) -> Result<crate::model::DraftRecord> {
        crate::gitbridge::check_git_head(self, line)?;
        let mut rec = crate::model::DraftRecord::new(op);
        rec.states = states;
        let working_after = match &working_before {
            Some(_) => self.after_state(ctx, line, op, &mut rec)?,
            None => None,
        };
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Move {
                    before: self.line_state(line, &line.head),
                    after: self.line_state(line, new_head),
                },
                working_before: working_before,
                working_after: working_after,
            },
        );
        rec.data = data;
        Ok(rec)
    }

    /// The `.git` folder of a line follows its head. A failure only warns: the move is done.
    pub fn after_head_moved(&self, line: &Line, new_head: &str) {
        let _t = crate::trace::span("refresh .git folder");
        let mut moved = line.clone();
        moved.head = new_head.to_string();
        if let Err(e) = crate::gitbridge::refresh_git_folder(self, &moved) {
            eprintln!("warning: .git folder of line {} not updated: {e:#}", line.name);
        }
    }

    /// The lock of `line` for a whole command: it moves the head of the line or writes its working
    /// folder. Take it before anything else, and never under the project lock (store.rs `LineLock`).
    /// Serialize cache builders and cache saves without blocking ordinary head saves.
    pub fn cache_build_lock(&self, line: &Line) -> Result<crate::store::LineLeaseSet> {
        self.p.line_lock(&format!("{}-cache", line.id), &format!("{} cache", line.name))
    }

    pub fn line_lock(&self, line: &Line) -> Result<crate::store::LineLeaseSet> {
        self.p.line_lock(&line.id, &line.name)
    }

    /// Run `commit` under the project lock, after a check that `line` still exists and has the head
    /// `head` that the caller started from. Only the records and the renames of a command go here.
    /// The caller holds the line lock, so the head can change only by a command that does not take
    /// it (line delete, follow, fetch). Such a change is refused: nothing is recorded, and the error
    /// names the move. The caller undoes its working folder change.
    pub fn commit_line<T>(
        &self,
        leases: &crate::store::LineLeaseSet,
        line: &Line,
        head: &str,
        commit: impl FnOnce(&crate::store::LockedProject<'_>, &Line) -> PublishResult<T>,
    ) -> PublishResult<T> {
        leases.require_line(&self.p, &line.id)?;
        // Test hooks (tests/t122-line-locks.sh), named by line: a crash after the folder was written
        // and before the record, and a head that another command moved.
        let hook = |name: &str| std::env::var(name).ok().is_some_and(|v| v == line.name);
        if hook("LAYR_TEST_CRASH_BEFORE_COMMIT") {
            std::process::exit(86);
        }
        // Test barrier: simulate a fetch changing the head after preparation, before commit.
        if let Some(dir) = std::env::var_os("LAYR_TEST_COMMIT_BARRIER") {
            let dir = PathBuf::from(dir);
            std::fs::write(dir.join("ready"), b"")?;
            let start = std::time::Instant::now();
            while !dir.join("release").exists() && start.elapsed() < std::time::Duration::from_secs(30) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let _lock = leases.lock(&self.p)?;
        let now = match self.p.db.line_by_id(&line.id)? {
            Some(l) => self.localize(l)?,
            None => return Err(line_gone(&line.name).into()),
        };
        if hook("LAYR_TEST_MOVE_HEAD_AT_COMMIT") {
            let parent = self.state(head).ok().and_then(|s| s.parents.first().cloned()).unwrap_or_else(|| head.to_string());
            return Err(line_moved(&line.name, head, &parent).into());
        }
        if now.head != head {
            return Err(line_moved(&line.name, head, &now.head).into());
        }
        commit(&_lock, &now)
    }

    /// A snapshot of the working content right after an operation, so undo can revert only the
    /// changes of that operation and keep later work.
    pub fn after_state(&self, ctx: &Ctx, line: &Line, op: &str, rec: &mut crate::model::DraftRecord) -> Result<Option<String>> {
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

    pub fn parents(&self, id: &SourceStateId) -> Result<Vec<String>> {
        Ok(self.source_state(id.as_str())?.parents)
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
                let dir = c.new().or(c.old()).is_some_and(|m| m.is_dir());
                !skip_path(c.path()) && !rules.ignored(c.path(), dir)
            })
            .map(|c| c.path().clone())
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
    pub fn merge_base(&self, a: &SourceStateId, b: &SourceStateId) -> Result<Option<String>> {
        let aa = self.ancestors(a.as_str())?;
        let bs: BTreeSet<String> = self.ancestors(b.as_str())?.into_iter().collect();
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
    pub fn merge_base(&self, a: &SourceStateId, b: &SourceStateId) -> Option<Option<String>> {
        self.merge_base_ids(a.as_str(), b.as_str())
    }

    fn merge_base_ids(&self, a: &str, b: &str) -> Option<Option<String>> {
        let aa = self.ancestors(vec![a])?;
        let bs: HashSet<&str> = self.ancestors(vec![b])?.into_iter().collect();
        let common: Vec<&str> = aa.into_iter().filter(|x| bs.contains(x)).collect();
        let seeds: Vec<&str> =
            common.iter().flat_map(|c| self.parents.get(*c).into_iter().flatten().map(String::as_str)).collect();
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
        let below: BTreeSet<String> =
            ancestors(common.iter().flat_map(|c| parents.get(c).cloned().unwrap_or_default()).collect()).into_iter().collect();
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
                    assert_eq!(h.merge_base_ids(a, b), Some(reference_merge_base(&parents, a, b)), "round {round}: {a} {b}");
                }
            }
        }
    }

    /// A walk that reaches an auxiliary state gives no answer, so the caller uses `Repo::merge_base`.
    #[test]
    fn an_auxiliary_state_in_the_walk_gives_no_answer() {
        let parents = HashMap::from([("top".to_string(), vec!["cache".to_string()]), ("cache".to_string(), vec![])]);
        let h = History { parents, auxiliary: HashSet::from(["cache".to_string()]) };
        assert_eq!(h.merge_base_ids("top", "top"), None);
        assert_eq!(h.merge_base_ids("cache", "cache"), None);
    }
}

/// Sort index-to-working changes into unstaged, untracked, and ignored paths.
fn classify(scan: &Path, list: Vec<EntryChange>, st: &mut Status) {
    classify_with(&Rules::new(scan), list, st);
}

fn classify_with(rules: &Rules, list: Vec<EntryChange>, st: &mut Status) {
    for c in changes::without_moves(list) {
        if skip_path(c.path()) {
            continue;
        }
        if c.old().is_none() {
            if rules.ignored(c.path(), false) {
                st.ignored.push(c.path().clone());
            } else {
                st.untracked.push(c.path().clone());
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
    if c.uses_markers() {
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
fn prune_empty_parents(target: &Path, to: &(impl fsutil::Src + ?Sized), rel: &str) -> Result<()> {
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

/// Normalize a private writable tree before publication, without following links or
/// entering nested storage roots. Matching entries keep their metadata and shared lowers.
/// The parent must remain private for the entire walk, including after the root is chowned.
pub(crate) fn give_tree(root: &Path, uid: u32, gid: u32) -> Result<()> {
    let _t = crate::trace::span(format!("give tree {}", root.display()));
    let tree = Tree::open(root)?;
    fsutil::walk(root, "", &mut |rel, m| {
        if fsutil::is_layer(root, rel, m) {
            return Ok(false);
        }
        if m.uid != uid || m.gid != gid {
            tree.chown(rel, uid, gid)?;
        }
        Ok(true)
    })?;
    let m = tree.stat_checked("")?;
    if m.uid != uid || m.gid != gid {
        tree.chown("", uid, gid)?;
    }
    Ok(())
}

/// The folders of the working folder `w` that `.layrattributes` files make `regenerable` and
/// that the ignore rules ignore. A regenerable folder that is not ignored is no cache: its files
/// are in history, and a cache must not hide them.
///
pub fn regenerable_folders(w: &Path) -> Result<Vec<String>> {
    // A cache that is taken is relocated in a private staging folder of the line (`.layr-cache-take-*`):
    // its folders are no caches of the line.
    let staged = |p: &String| p.split('/').next().is_some_and(|c| c.starts_with(".layr-cache-take-"));
    if matches!(crate::fs::host().observations.read_policy(), crate::fs::observations::ReadPolicy::Validate) {
        if let Ok(scan) = crate::fs::observe_tree(w) {
            if scan.values().all(FileFingerprint::verifiable) {
                return Ok(regenerable_in_scan(w, &scan).into_iter().filter(|p| !staged(p)).collect());
            }
        }
    }
    Ok(regenerable_by_walk(w)?.into_iter().filter(|p| !staged(p)).collect())
}

/// Literal declarations can establish a cold cache before the first build. Globs still discover
/// existing folders, but unknown output matching one must be refused rather than labelled.
fn literal_regenerable(w: &Path, dirs: &std::collections::HashSet<String>) -> Vec<String> {
    let (regenerable, ignored) = (Rules::limited(w, Some("regenerable"), dirs.clone()), Rules::limited(w, None, dirs.clone()));
    let mut found = Vec::new();
    for dir in dirs {
        let text = std::fs::read_to_string(w.join(dir).join(".layrattributes")).unwrap_or_default();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next() else { continue };
            if pattern.starts_with('#') || pattern.contains(['*', '?', '[', '\u{005c}']) || !words.any(|a| a == "regenerable") {
                continue;
            }
            let pattern = pattern.trim_matches('/');
            let rel = if dir.is_empty() { pattern.to_string() } else { format!("{dir}/{pattern}") };
            if crate::overlay::folder_path(&rel).is_ok() && regenerable.ignored(&rel, true) && ignored.ignored(&rel, true) {
                let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                if parent.is_empty() || !ignored.ignored(parent, true) {
                    found.push(rel);
                }
            }
        }
    }
    found
}

/// `regenerable_folders` from a scan of the working folder `w` (every entry, folders and rule
/// files; a layer is listed without its content). A folder inside an ignored folder is no
/// candidate, as the walk does not enter ignored folders.
fn regenerable_in_scan(w: &Path, scan: &BTreeMap<String, FileFingerprint>) -> Vec<String> {
    let mut rule_dirs = std::collections::HashSet::from([String::new()]);
    for (rel, e) in scan {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        if !matches!(e,FileFingerprint::Verified{stat,..} if stat.mode&0o170000==0o040000)
            && matches!(name, ".gitignore" | ".layrignore" | ".layrattributes")
        {
            rule_dirs.insert(dir.to_string());
        }
    }
    let mut found = literal_regenerable(w, &rule_dirs);
    let (regenerable, ignored) = (Rules::limited(w, Some("regenerable"), rule_dirs.clone()), Rules::limited(w, None, rule_dirs));
    for (rel, e) in scan {
        if rel.is_empty()
            || !matches!(e,FileFingerprint::Verified{stat,..} if stat.mode&0o170000==0o040000)
            || rel == ".git"
            || rel.starts_with(".git/")
        {
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
    found.dedup();
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
    children.extend(literal_regenerable(w, &rule_dirs));
    let (regenerable, ignored) = (Rules::limited(w, Some("regenerable"), rule_dirs.clone()), Rules::limited(w, None, rule_dirs));
    let mut found: Vec<String> = children
        .into_iter()
        .filter(|rel| {
            let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            (parent.is_empty() || !ignored.ignored(parent, true)) && regenerable.ignored(rel, true) && ignored.ignored(rel, true)
        })
        .collect();
    found.sort();
    found.dedup();
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
            let cloned = t.clone_new_file(&src, d)?;
            let (dst, cloned) = match cloned {
                Some(dst) => (dst, true),
                None => (t.create(d, m.mode)?, false),
            };
            if !cloned {
                layr_fs_unix::copy_contents(&src, &dst)?;
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
                    crate::byte_size::human_size(tree_bytes(&t.path.join(&old)))
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
/// copied. The automatic save skips it; `save --overlays` and `overlay repair` report it.
#[derive(Debug)]
pub enum CacheRefused {
    Busy { path: String, user: crate::busy::User },
    Changed { path: String },
}

impl std::fmt::Display for CacheRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { path, user } => {
                write!(f, "{path}: not made a cache: in use by {user}; run again when the build ends")
            }
            Self::Changed { path } => {
                write!(f, "{path}: not made a cache: written while it was copied; run again when the build ends")
            }
        }
    }
}

impl std::error::Error for CacheRefused {}

/// Refuse a folder that a process can write: a copy and a swap would lose the writes that come
/// later. A reader keeps its old files and loses nothing.
fn refuse_busy(w: &Path, path: &str) -> Result<()> {
    if let Some(user) = crate::busy::writer_of(&fsutil::safe_join(w, path)?)? {
        return Err(CacheRefused::Busy { path: path.to_string(), user }.into());
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

/// `holds_files` for a tree to read (`Snap`): on APFS the layers of a thin state, with no
/// complete copy of it.
pub fn holds_files_in(tree: &Snap, path: &str) -> bool {
    use layr_fs_core::{RelDir, RelPath, TreeRead};
    if let Some(root) = tree.folder() {
        return holds_files(root, path);
    }
    let Ok(at) = RelDir::from_bytes(path.as_bytes()) else { return true };
    for (end, _) in path.match_indices('/') {
        let Ok(parent) = RelDir::from_bytes(path[..end].as_bytes()) else { return true };
        match tree.kind(&parent) {
            Ok(Some(kind)) if kind != fsutil::FType::Dir => return true,
            Err(_) => return true,
            _ => {}
        }
    }
    match tree.kind(&at) {
        Ok(None) => return false,
        Ok(Some(kind)) if kind != fsutil::FType::Dir => return true,
        Err(_) => return true,
        _ => {}
    }
    let mut found = false;
    if layr_fs_common::walk(tree, &at, &mut |_: &RelPath, entry| {
        found |= entry.kind != fsutil::FType::Dir;
        Ok(!found)
    })
    .is_err()
    {
        return true;
    }
    found
}

/// Whether the tree at `root` has a file, link or other entry that is not a folder at `path` or
/// below it (a folder of a state that holds versioned files). A path below such an entry counts;
/// an error counts too, so that a folder whose files cannot be read never becomes a cache.
/// Whether a file or folder below `root` (or `root` itself) changed after `since_ms`: its
/// modification time (in nanoseconds) is not older. A new, changed, deleted or renamed file changes
/// the time of the file or of its folder. It does not follow links or enter other file systems.
pub fn folder_changed_since(root: &Path, since_ms: i64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let since = i128::from(since_ms) * 1_000_000;
    let newer = |m: &std::fs::Metadata| i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()) >= since;
    let Ok(top) = std::fs::symlink_metadata(root) else { return true };
    if newer(&top) {
        return true;
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { return true };
        for e in rd {
            let Ok(e) = e else { return true };
            if d == root && e.file_name() == crate::cache_overlay::PROVENANCE {
                continue;
            }
            let Ok(m) = e.metadata() else { return true };
            if newer(&m) {
                return true;
            }
            if m.is_dir() && m.dev() == top.dev() {
                dirs.push(e.path());
            }
        }
    }
    false
}

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
    fn literal_cache_declarations_include_absent_paths_but_respect_negation() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-literal-cache-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join(".gitignore"), "target/\nignored/\n")?;
        std::fs::write(
            root.join(".layrattributes"),
            "target regenerable\nignored regenerable\nignored -regenerable\n*cache regenerable\n",
        )?;
        assert_eq!(literal_regenerable(&root, &std::collections::HashSet::from([String::new()])), ["target"]);
        assert!(!root.join("target").exists(), "classification does not itself label bytes");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

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
        assert_eq!(found, vec!["a/target", "b/node_modules", "c/dist", "d/target", "target"]);
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

    #[test]
    fn a_new_command_does_not_read_the_list_of_an_earlier_command() -> Result<()> {
        let w = Path::new("/layr-test/regenerable-memo/w");
        REGENERABLE.with_borrow_mut(|memo| *memo = Some((w.to_path_buf(), vec!["target".to_string()])));
        assert!(REGENERABLE.with_borrow(|memo| memo.is_some()));
        // The same path gives the list it holds, with no walk, inside one command.
        assert_eq!(regenerable_memo(w)?, vec!["target".to_string()]);
        // The dispatcher (`cmd::run`, `service::run_as`) starts every command with `begin_command`.
        begin_command();
        assert!(REGENERABLE.with_borrow(|memo| memo.is_none()));
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
            author: Default::default(),
            captures: Default::default(),
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
        if !crate::fs::host().namespace.supported(&root) {
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
        if !crate::fs::host().namespace.supported(&root) {
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
                    "-q"
                ]
            )?,
            0
        );
        std::fs::write(ctx.cwd.join(".gitignore"), "cache/\n")?;
        // Establish this location before simulating a tool recreating a plain cache folder.
        let provenance = {
            let repo = Repo::open(&ctx, "concurrent")?;
            repo.initialize_missing_caches(&repo.line("main")?)?;
            std::fs::read(ctx.cwd.join("cache/.layr-cache-location"))?
        };
        crate::fs::remove(&ctx.cwd.join("cache"))?;
        std::fs::create_dir(ctx.cwd.join("cache"))?;
        std::fs::write(ctx.cwd.join("cache/.layr-cache-location"), provenance)?;
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
        // Best effort, as the other tests here: without root a read-only state subvolume stays.
        let _ = crate::fs::remove(&root.join("store"));
        let _ = std::fs::remove_dir_all(&root);
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
        let socket = root.0.join("source/private/socket");
        // Relative sockaddr_un paths avoid the macOS length limit in deep check folders.
        let cwd = std::env::current_dir()?;
        let _socket = UnixListener::bind(socket.strip_prefix(&cwd).unwrap_or(&socket))?;
        let tree = Tree::open(&root.0)?;
        assert!(copy_tree(&tree, "source", "destination", None, &mut std::collections::HashMap::new()).is_err());
        assert_eq!(std::fs::metadata(root.0.join("destination/private"))?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }
}

#[cfg(test)]
mod cache_platform_tests {
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
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db")).unwrap();
        std::fs::create_dir(project_dir.join("states")).unwrap();
        let id = ids::new_id();
        let p = Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut rec = crate::model::DraftRecord::new("project");
        rec.data = serde_json::json!({"id":id,"name":"p","config":{}});
        p.lock_maintenance().unwrap().append(rec, &Actor { uid: 0, name: "root".into(), session: None }).unwrap();
        let (first, second, tip, cycle) = (ids::new_id(), ids::new_id(), ids::new_id(), ids::new_id());
        let mut source_record = crate::model::DraftRecord::new("state");
        for (id, parents) in
            [(&first, vec![]), (&second, vec![]), (&tip, vec![first.clone(), second.clone()]), (&cycle, vec![cycle.clone()])]
        {
            source_record.states.push(
                serde_json::from_value(serde_json::json!({
                    "id":id,"kind":"commit","parents":parents,"subvol":"","time":1,
                    "author":{"name":"test","email":"test@example.com"}
                }))
                .unwrap(),
            );
        }
        p.lock_maintenance().unwrap().append(source_record, &ctx.actor()).unwrap();
        {
            let repo = Repo::open(&ctx, "p").unwrap();
            assert!(matches!(crate::revs::resolve(&repo, None, &tip).unwrap(), ResolvedRevision::Source(_)));
            for (suffix, expected) in [("~", &first), ("^2", &second), ("^0", &tip), ("~0", &tip)] {
                assert_eq!(crate::revs::resolve_state(&repo, None, &format!("{tip}{suffix}")).unwrap(), *expected);
            }
            for (suffix, message) in [("~2", "state has no parent"), ("^3", "state has no parent 3")] {
                let rev = format!("{tip}{suffix}");
                assert_eq!(crate::revs::resolve_state(&repo, None, &rev).unwrap_err().to_string(), format!("{rev}: {message}"));
            }
            let rev = format!("{cycle}~2");
            assert_eq!(crate::revs::resolve_state(&repo, None, &rev).unwrap_err().to_string(), format!("bad revision {rev}"));
            let view = crate::overlay::View::new(tip.clone(), vec![]);
            let view_id = view.id().unwrap();
            let mut saved = crate::model::DraftRecord::new("view.save");
            saved.data = serde_json::json!({"view_id":view_id,"view":view});
            p.lock_maintenance().unwrap().append(saved, &ctx.actor()).unwrap();
            let rev = format!("views/{view_id}");
            assert!(matches!(crate::revs::resolve(&repo, None, &rev).unwrap(), ResolvedRevision::View(_)));
            assert_eq!(crate::revs::resolve_state(&repo, None, &rev).unwrap(), tip);
            assert_eq!(crate::revs::resolve(&repo, None, &rev).unwrap().into_source(&repo).unwrap().as_str(), tip);
        }
        for kind in [StateKind::Cache, StateKind::Derived] {
            let id = ids::new_id();
            // A state is a subvolume where the store can make one (btrfs): a plain folder there reads as a rollback.
            #[cfg(not(feature = "overlayfs"))]
            let made = crate::fs::create(&p.state_path(&id)).is_ok();
            #[cfg(feature = "overlayfs")]
            let made = false;
            if !made {
                std::fs::create_dir(p.state_path(&id)).unwrap();
            }
            let mut rec = crate::model::DraftRecord::new("state");
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
                tree: None,
                times: None,
                cache_key_parts: Default::default(),
                parts: Default::default(),
            });
            p.lock_maintenance().unwrap().append(rec, &ctx.actor()).unwrap();
            let repo = Repo::open(&ctx, "p").unwrap();
            assert!(repo.state_root(&id).is_ok());
            assert_eq!(crate::revs::resolve_state(&repo, None, &id).unwrap(), id);
            for args in [vec!["line", "new", "rejected", &id], vec!["diff", &id, &id], vec!["log", &id], vec!["show", &id]] {
                let args: Vec<String> = args.into_iter().map(String::from).collect();
                let error = crate::cmd::run(&ctx, &args).unwrap_err().to_string();
                assert!(error.contains("outside source history"), "{args:?}: {error}");
            }
            assert!(repo.source_id(&id).unwrap_err().to_string().contains("outside source history"));
            assert!(crate::gitbridge::export_commit(&repo, &id).unwrap_err().to_string().contains("outside source history"));
            assert!(matches!(crate::revs::resolve(&repo, None, &id).unwrap(), ResolvedRevision::Auxiliary(_)));
            for suffix in ["^0", "~0"] {
                assert_eq!(crate::revs::resolve_state(&repo, None, &format!("{id}{suffix}")).unwrap(), id);
            }
            for suffix in ["^", "~"] {
                assert!(crate::revs::resolve_state(&repo, None, &format!("{id}{suffix}"))
                    .unwrap_err()
                    .to_string()
                    .contains("outside source history"));
            }
            let args = vec!["ref".into(), "set".into(), format!("caches/{}", crate::db::kind_str(kind)), id.clone()];
            assert_eq!(crate::cmd::run(&ctx, &args).unwrap(), 0);
        }
        drop(p);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// An index choice whose presence and source are shared by its token and comparison.
struct SelectedIndex {
    source: Snap,
    stage: Option<SelectedStage>,
}

struct SelectedStage {
    path: PathBuf,
    identity: Vec<u8>,
    root: FileFingerprint,
    observed_before: u64,
}

impl SelectedStage {
    fn capture(path: PathBuf) -> Result<Self> {
        let observed_before = crate::status_cache::clock()?;
        let root = FileFingerprint::Verified {
            stat: crate::status_cache::FileStat::of(&std::fs::symlink_metadata(&path)?),
            evidence: None,
        };
        let identity = crate::fs::host().observations.stage_identity(&path)?;
        Ok(Self { path, identity, root, observed_before })
    }
}

impl SelectedIndex {
    fn validate_stage(&self) -> Result<()> {
        if let Some(stage) = &self.stage {
            let root = FileFingerprint::Verified {
                stat: crate::status_cache::FileStat::of(&std::fs::symlink_metadata(&stage.path)?),
                evidence: None,
            };
            if root != stage.root || crate::fs::host().observations.stage_identity(&stage.path)? != stage.identity {
                bail!("index stage was replaced while status was being read");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static INDEX_SELECTED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// The head and the index content of a line (`Repo::index_token`).
pub type IndexToken = String;

pub struct LeftOutComparison {
    started: u64,
    entries: BTreeMap<String, FileFingerprint>,
    local: Vec<u8>,
    st: Status,
}

/// Identity of every mutable input used by a comparison.
struct ObservedVersion {
    line: String,
    head: String,
    index: IndexToken,
    ignore_policy: Vec<u8>,
    started: u64,
    entries: BTreeMap<String, FileFingerprint>,
}

/// A comparison owns the frozen bytes it describes. It grants no publication rights.
#[must_use]
pub struct PreparedComparison {
    observed: ObservedVersion,
    st: Status,
    capture: ComparisonCapture,
}

enum ComparisonCapture {
    Snapshot { scan: Temp, index_temp: Option<Temp>, index: Snap },
    Report,
}

/// Publication authority borrows the project lock and owns exactly one capture.
#[must_use]
pub struct ValidatedForPublish<'lock, 'p, Capture> {
    _locked: &'lock crate::store::LockedProject<'p>,
    prepared: Capture,
}

impl ObservedVersion {
    fn matches(&self, repo: &Repo) -> Result<bool> {
        let line = repo.line(&self.line)?;
        let work = repo.working(&line);
        Ok(line.head == self.head
            && repo.index_token(&line)? == self.index
            && Rules::new(&work).local_policy()? == self.ignore_policy
            && self.entries.values().all(|entry| entry.trusted(self.started))
            && crate::fs::observe_tree(&work)? == self.entries)
    }
}

#[must_use]
pub struct PreparedAutomatic<'r, 's> {
    repo: &'r Repo<'s>,
    folder: Option<PreparedFolder>,
}

struct PreparedFolder {
    observed: ObservedVersion,
    prev: String,
    snapshot: AutomaticState,
    _pin: crate::free::Pin,
}

enum AutomaticState {
    Unchanged,
    Snapshot { state: StateRec, comparison: Option<crate::objstore::Compared> },
}

impl Drop for PreparedAutomatic<'_, '_> {
    fn drop(&mut self) {
        if let Some(PreparedFolder { snapshot: AutomaticState::Snapshot { state, .. }, .. }) = &self.folder {
            self.repo.discard_states(std::slice::from_ref(state));
        }
    }
}

impl PreparedAutomatic<'_, '_> {
    pub fn publish(
        self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        running: bool,
    ) -> Result<Option<StateRec>> {
        locked.check(&self.repo.p)?;
        #[cfg(target_os = "macos")]
        if crate::views::of(self.repo, line).is_some() {
            return self.repo.save(locked, ctx, line, message, running, false);
        }
        let Some(folder) = &self.folder else {
            return self.repo.save(locked, ctx, line, message, running, false);
        };
        let observed = &folder.observed;
        if observed.line != line.id
            || observed.head != line.head
            || self.repo.line(&line.id)?.head != line.head
            || self.repo.save_base(line)? != folder.prev
        {
            return self.repo.save(locked, ctx, line, message, running, false);
        }
        if !observed.matches(self.repo)? {
            let _fallback = crate::trace::span("prepared save invalidated");
            if matches!(folder.snapshot, AutomaticState::Snapshot { .. })
                && crate::source::storage::sparse_format(&self.repo.p).is_some()
                && self.repo.layers(line)?.is_empty()
                && self.repo.private_paths(line)?.is_empty()
                && self.repo.plain_caches(line)?.is_empty()
            {
                // The retained capture is still an honest automatic state. Publishing it first
                // gives the safe save a close base, so only concurrent writes are recopied.
                let _layer = crate::trace::span("layer over prepared save");
                let repo = self.repo;
                let retained = ValidatedForPublish { _locked: locked, prepared: self }.publish(ctx, line, message, running)?;
                return Ok(repo.save(locked, ctx, line, message, running, false)?.or(retained));
            }
            return self.repo.save(locked, ctx, line, message, running, false);
        }
        let _taken = crate::trace::span("take prepared save");
        ValidatedForPublish { _locked: locked, prepared: self }.publish(ctx, line, message, running)
    }

    pub fn working_state(
        self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        message: &str,
    ) -> Result<Option<String>> {
        let repo = self.repo;
        Ok(Some(match self.publish(locked, ctx, line, message, false)? {
            Some(state) => state.id,
            None => repo.save_base(line)?,
        }))
    }
}

impl ValidatedForPublish<'_, '_, PreparedAutomatic<'_, '_>> {
    fn publish(self, ctx: &Ctx, line: &Line, message: &str, running: bool) -> Result<Option<StateRec>> {
        let mut prepared = self.prepared;
        let folder = prepared.folder.as_mut().context("missing prepared folder")?;
        let of = match &folder.snapshot {
            AutomaticState::Snapshot { state, .. } => state.id.clone(),
            AutomaticState::Unchanged => folder.prev.clone(),
        };
        if let AutomaticState::Snapshot { state, comparison } = &mut folder.snapshot {
            if let Some(compared) = comparison.take() {
                compared.record(&prepared.repo.p, state)?;
            }
            state.message = message.to_string();
            state.running = running || crate::fsutil::take_unsettled();
            let before = prepared.repo.recorded_working(line)?;
            let mut rec = crate::model::DraftRecord::new("save");
            rec.states.push(state.clone());
            rec.data =
                serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": state.id});
            // Publication errors carry their commit resolution; commit_record owns cleanup.
            let result = prepared.repo.commit_record(self._locked, ctx, rec);
            if result.is_err() {
                folder.snapshot = AutomaticState::Unchanged;
                result?;
            }
        }
        if let Err(e) = prepared.repo.save_cache_states(self._locked, ctx, line, &of) {
            eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
        }
        clear_regenerable_memo();
        Ok(match std::mem::replace(&mut folder.snapshot, AutomaticState::Unchanged) {
            AutomaticState::Snapshot { state, .. } => Some(state),
            AutomaticState::Unchanged => None,
        })
    }
}

impl PreparedComparison {
    pub fn needs_save(&self, repo: &Repo, line: &Line) -> Result<bool> {
        let st = &self.st;
        Ok(!st.clean() || !st.untracked.is_empty() || !st.ignored.is_empty() || repo.save_base(line)? != line.head)
    }

    pub fn revalidate<'lock, 'p>(
        self,
        repo: &Repo,
        locked: &'lock crate::store::LockedProject<'p>,
    ) -> Result<Option<ValidatedForPublish<'lock, 'p, Self>>> {
        locked.check(&repo.p)?;
        if !self.observed.matches(repo)? {
            let _invalidated = crate::trace::span("prepared comparison invalidated");
            return Ok(None);
        }
        Ok(Some(ValidatedForPublish { _locked: locked, prepared: self }))
    }
}

impl ValidatedForPublish<'_, '_, PreparedComparison> {
    /// Reuse a clean head comparison for the pre-command save. Ignored additions still
    /// require an automatic state, even though the displayed status calls them clean.
    pub fn working_state(
        self,
        repo: &Repo,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        automatic: Option<PreparedAutomatic<'_, '_>>,
    ) -> Result<(Option<String>, Status)> {
        let st = &self.prepared.st;
        let unchanged = st.clean() && st.untracked.is_empty() && st.ignored.is_empty() && repo.save_base(line)? == line.head;
        let working = if unchanged {
            if let Err(e) = repo.save_cache_states(self._locked, ctx, line, &line.head) {
                eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
            }
            clear_regenerable_memo();
            Some(line.head.clone())
        } else {
            match automatic {
                Some(prepared) => prepared.working_state(self._locked, ctx, line, message)?,
                None => repo.working_state(self._locked, ctx, line, message)?,
            }
        };
        Ok((working, self.prepared.st))
    }
}

/// `save --overlays` work validated again under the project lock.
pub struct PreparedCaches {
    head: String,
    caches: Vec<String>,
    scan: BTreeMap<String, FileFingerprint>,
    st: StateRec,
    same: bool,
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
        failed_commit_keeps_content(false, false)
    }

    #[test]
    fn object_state_survives_commit_error_and_deferred_native_cleanup() -> Result<()> {
        failed_commit_keeps_content(true, false)
    }

    #[test]
    fn an_unknown_commit_retains_snapshot_and_working_write() -> Result<()> {
        failed_commit_keeps_content(false, true)
    }

    fn failed_commit_keeps_content(objects: bool, unknown: bool) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-commit-record-{}", ids::new_id()));
        let ctx = Ctx {
            store: Store::open(&dir)?,
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db"))?;
        std::fs::create_dir(project_dir.join("states"))?;
        let id = ids::new_id();
        let p = Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite"))?,
        };
        let mut rec = crate::model::DraftRecord::new("project");
        rec.data = serde_json::json!({"id": id, "name": "p", "config": {"objects.enabled": objects}});
        p.lock_maintenance().unwrap().append(rec, &Actor { uid: 0, name: "root".into(), session: None })?;
        let repo = Repo { p, ctx: None };
        let state = ids::new_id();
        let preparation_pin = crate::free::prepare_state(&repo.p, &state)?;
        std::fs::create_dir(repo.p.state_path(&state))?;
        std::fs::write(repo.p.state_path(&state).join("file"), b"committed content")?;
        let mut rec = crate::model::DraftRecord::new("state");
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
            tree: None,
            times: None,
            cache_key_parts: Default::default(),
            parts: Default::default(),
        });
        rec.lines.insert(
            ids::new_id(),
            crate::model::LineChange {
                transition: crate::model::LineTransition::Create {
                    after: LineState {
                        name: "main".into(),
                        head: state.clone(),
                        owner: ctx.caller.uid,
                        machine: ctx.store.machine.id.clone(),
                    },
                },
                working_before: None,
                working_after: None,
            },
        );
        let before = dir.join("before");
        let after = dir.join("after");
        let work = dir.join("working");
        for path in [&before, &after, &work] {
            std::fs::create_dir(path)?;
        }
        std::fs::write(before.join("file"), b"before")?;
        for path in [&after, &work] {
            std::fs::write(path.join("file"), b"after")?;
        }
        let mut write = WorkingWrite::new(&repo, work.clone(), None, snap(&before), snap(&after), None);
        crate::db::FAIL_AFTER_COMMIT.with(|f| f.set(true));
        crate::db::FAIL_RECONCILE.with(|f| f.set(unknown));
        let error = repo.commit_record(&repo.p.lock_maintenance().unwrap(), &ctx, rec).unwrap_err();
        if unknown {
            assert!(matches!(error, CommitResolution::Unknown(_)));
        } else {
            assert!(matches!(error, CommitResolution::Committed(_)));
        }
        write.publication_failed(&error);
        drop(write);
        assert_eq!(std::fs::read(work.join("file"))?, b"after");
        assert!(repo.p.db.state(&state)?.is_some(), "the database holds the record");
        drop(preparation_pin);
        assert!(!repo.p.db.queued_frees()?.contains(&state), "releasing a preparation must retain an ambiguous publication");
        assert!(repo.p.has_state(&state), "a state that the record names keeps its snapshot");
        let mut duplicate = crate::model::DraftRecord::new("state");
        duplicate.states.push(repo.p.db.state(&state)?.unwrap());
        let mut unrecorded = duplicate.states[0].clone();
        unrecorded.id = ids::new_id();
        let unrecorded_path = repo.p.state_path(&unrecorded.id);
        std::fs::create_dir(&unrecorded_path)?;
        std::fs::write(unrecorded_path.join("file"), b"uncommitted")?;
        duplicate.states.push(unrecorded);
        let duplicate_error = repo.commit_record(&repo.p.lock_maintenance()?, &ctx, duplicate).unwrap_err();
        assert!(matches!(duplicate_error, CommitResolution::NotCommitted(_)));
        assert!(repo.p.has_state(&state), "rejecting a reused ID must retain its already-recorded snapshot");
        assert!(!unrecorded_path.exists(), "only the snapshot with no committed record is discarded");
        if objects {
            let before = repo.state_src(&state)?;
            assert_eq!(fsutil::read_file(&before, "file")?, b"committed content");
            let pin = repo.pin(&[&state])?;
            crate::objstore::retire_native(&repo)?;
            assert!(repo.p.has_state(&state), "a native reader keeps its copy");
            drop(pin);
            crate::objstore::retire_native(&repo)?;
            assert!(!repo.p.has_state(&state), "cleanup can remove only the native copy");
            let after = repo.state_src(&state)?;
            assert_eq!(fsutil::read_file(&after, "file")?, b"committed content");
            crate::objstore::retire_native(&repo)?;
            assert_eq!(fsutil::read_file(&after, "file")?, b"committed content");
        }
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

#[cfg(test)]
mod owner_walk_tests {
    use super::{give_tree, owner_walk_skippable};
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn matching_entries_are_not_chowned_and_symlinks_are_not_followed() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-owner-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("tree/sub"))?;
        std::fs::write(root.join("tree/sub/file"), b"content")?;
        std::fs::write(root.join("outside"), b"outside")?;
        std::fs::hard_link(root.join("tree/sub/file"), root.join("tree/twin"))?;
        std::os::unix::fs::symlink("../../outside", root.join("tree/sub/link"))?;
        let paths = ["tree", "tree/sub", "tree/sub/file", "tree/twin", "tree/sub/link", "outside"];
        let before: Vec<_> = paths.iter().map(|p| std::fs::symlink_metadata(root.join(p))).collect::<std::io::Result<_>>()?;
        give_tree(&root.join("tree"), before[0].uid(), before[0].gid())?;
        for (path, old) in paths.iter().zip(&before) {
            let new = std::fs::symlink_metadata(root.join(path))?;
            assert_eq!(
                (new.uid(), new.gid(), new.ino(), new.ctime(), new.ctime_nsec()),
                (old.uid(), old.gid(), old.ino(), old.ctime(), old.ctime_nsec()),
                "{path}"
            );
        }
        assert!(give_tree(&root.join("tree/sub/link"), before[0].uid(), before[0].gid()).is_err());
        assert_eq!(std::fs::read(root.join("outside"))?, b"outside");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn matching_root_does_not_hide_a_different_entry_group() -> anyhow::Result<()> {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        // Changing a file to a supplementary group needs no privileged test process.
        let Some(other) = rustix::process::getgroups()?.into_iter().find(|g| g.as_raw() != gid) else {
            return Ok(());
        };
        let root = std::env::temp_dir().join(format!("layr-owner-group-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("file"), b"content")?;
        std::os::unix::fs::symlink("file", root.join("link"))?;
        let tree = crate::tree::Tree::open(&root)?;
        tree.chown("file", uid, other.as_raw())?;
        tree.chown("link", uid, other.as_raw())?;
        give_tree(&root, uid, gid)?;
        for path in ["", "file", "link"] {
            let m = tree.stat_checked(path)?;
            assert_eq!((m.uid, m.gid), (uid, gid));
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

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

#[cfg(test)]
mod status_policy_tests {
    use super::*;

    #[test]
    fn classification_uses_captured_policy_and_validates_after_consuming_it() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-status-policy-{}", ids::new_id()));
        std::fs::create_dir_all(root.join(".git/info"))?;
        std::fs::write(root.join("file"), b"new")?;
        let exclude = root.join(crate::ignore_rules::LOCAL_EXCLUDE);
        std::fs::write(&exclude, b"")?;
        let add = || EntryChange::Add {
            path: "file".into(),
            new: fsutil::meta_of(&std::fs::symlink_metadata(root.join("file")).unwrap()),
        };

        // Policy A is captured for the key. B arrives exactly where the old code
        // reopened rules in classify; restoring A must not leave B's cached result.
        let policy_a = Rules::new(&root).local_policy()?;
        let rules = Rules::captured(&root, std::iter::once("file"), &policy_a)?;
        std::fs::write(&exclude, b"file\n")?;
        let mut st = Status::default();
        classify_with(&rules, vec![add()], &mut st);
        assert_eq!(st.untracked, ["file"]);
        assert!(st.ignored.is_empty());
        assert!(!rules.captured_matches()?);
        std::fs::write(&exclude, b"")?;
        assert!(rules.captured_matches()?);

        // An absent versioned ignore file is also an input. Appearance after capture
        // invalidates it even though the changed-file candidate set held no rule file.
        std::fs::write(root.join(".gitignore"), b"file\n")?;
        assert!(!rules.captured_matches()?);
        let rules = Rules::captured(&root, std::iter::once("file"), &policy_a)?;
        std::fs::write(root.join(".gitignore"), b"")?;
        let mut st = Status::default();
        classify_with(&rules, vec![add()], &mut st);
        assert_eq!(st.ignored, ["file"]);
        assert!(!rules.captured_matches()?);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn status_cache_keeps_the_selected_index_when_a_stage_appears_then_disappears() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-status-index-{}", ids::new_id()));
        let store = crate::store::Store::open(&root.join("store"))?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        std::fs::create_dir_all(dir.join("meta/stage"))?;
        let repo = Repo {
            p: Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))?,
            },
            ctx: None,
        };
        let line = Line {
            id: ids::new_id(),
            name: "main".into(),
            head: ids::new_id(),
            owner: rustix::process::geteuid().as_raw(),
            machine: store.machine.id.clone(),
        };
        let head = root.join("head");
        let work = root.join("work");
        std::fs::create_dir(&head)?;
        std::fs::create_dir(&work)?;
        std::fs::write(head.join("file"), b"base")?;
        std::fs::write(work.join("file"), b"edit")?;
        assert!(repo.current_stage(&line)?.is_none());
        let selected = SelectedIndex { source: snap(&head), stage: None };
        let key = repo.status_key(&line, &work, &selected, b"")?.unwrap();

        // add creates a stage after the no-stage key was read. The old independent
        // index_view lookup would now choose this staged content for cache seeding.
        let stage = repo.p.stage_path(&line.id);
        std::fs::create_dir(&stage)?;
        std::fs::write(stage.join("file"), b"edit")?;
        repo.mark_stage(&line)?;
        assert!(repo.current_stage(&line)?.is_some());
        let (frozen, _temp) = repo.snapshot_index(&selected)?;
        assert!(same_tree(&selected.source, &frozen));

        // A mixed reset removes the stage before validation: simply rechecking the
        // current stage would see None again, so the actual captured index must bind it.
        std::fs::remove_dir_all(&stage)?;
        std::fs::remove_file(repo.stage_base_file(&line))?;
        assert!(repo.current_stage(&line)?.is_none());
        assert_eq!(
            repo.status_key(&line, &work, &SelectedIndex { source: selected.source.clone(), stage: None }, b"")?,
            Some(key)
        );
        let candidate = EntryChange::observed(
            "file".into(),
            fsutil::lmeta(&frozen, "file"),
            fsutil::lmeta(&work, "file"),
            crate::compare::DataEvidence::Unknown,
        )
        .unwrap();
        let list = changes::file_changes_of(&frozen, &work, vec![candidate])?;
        let mut st = Status::default();
        classify(&work, list, &mut st);
        assert!(st.staged.is_empty());
        assert_eq!(st.unstaged.len(), 1);
        assert_eq!(st.unstaged[0].path(), "file");
        drop(repo);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn prepared_status_reuses_one_index_and_rejects_replacement() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-prepared-index-{}", ids::new_id()));
        std::fs::create_dir_all(&root)?;
        if !crate::fs::host().namespace.supported(&root) {
            std::fs::remove_dir_all(root).context("remove fixture")?;
            return Ok(());
        }
        let store = crate::store::Store::open(&root.join("store"))?;
        let dir = store.project_dir("p")?;
        for path in ["meta/db", "meta/stage", "meta/scan", "meta/work", "lines"] {
            std::fs::create_dir_all(dir.join(path))?;
        }
        // A frozen publication needs a private parent (mode 0700); create_dir_all gives 0755 with the usual umask.
        for path in ["", "meta", "meta/db", "meta/stage", "meta/scan", "meta/work", "lines"] {
            std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(0o700))?;
        }
        let repo = Repo {
            p: Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))?,
            },
            ctx: None,
        };
        let actor = crate::model::Actor { uid: rustix::process::geteuid().as_raw(), name: "Test".into(), session: None };
        let mut rec = crate::model::DraftRecord::new("project");
        rec.data = serde_json::json!({"id": repo.p.id, "name": "p", "config": {"objects.enabled": true}});
        repo.p.lock_maintenance()?.append(rec, &actor)?;
        let line = Line {
            id: ids::new_id(),
            name: "main".into(),
            head: ids::new_id(),
            owner: actor.uid,
            machine: store.machine.id.clone(),
        };
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("file"), b"tracked")?;
        let objects = crate::objstore::open(&repo.p)?;
        let written = layr_objects::write::write_tree(
            &objects,
            &source,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let mut rec = crate::model::DraftRecord::new("state");
        rec.states.push(serde_json::from_value(serde_json::json!({
            "id": line.head, "kind": "commit", "subvol": "", "author": crate::model::Author::default(),
            "time": ids::now_ms(), "tree": written.tree.unwrap().to_string(), "times": written.times.unwrap().to_string(),
        }))?);
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Create {
                    after: LineState {
                        name: line.name.clone(),
                        head: line.head.clone(),
                        owner: line.owner,
                        machine: line.machine.clone(),
                    },
                },
                working_before: None,
                working_after: None,
            },
        );
        repo.p.lock_maintenance()?.append(rec, &actor)?;
        let work = repo.working(&line);
        crate::fs::create(&work)?;
        std::fs::write(work.join("file"), b"tracked")?;
        std::fs::create_dir_all(work.join(".git/info"))?;
        std::fs::write(work.join(crate::ignore_rules::LOCAL_EXCLUDE), b"")?;
        for report in [false, true] {
            let stage = repo.p.stage_path(&line.id);
            let base = repo.stage_base_file(&line);
            let hook_stage = stage.clone();
            let hook_base = base.clone();
            let head = line.head.clone();
            INDEX_SELECTED_HOOK.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    // Equivalent to rm --cached: create an index without the tracked file
                    // after the public preparation selected the original no-stage head.
                    crate::fs::create(&hook_stage).unwrap();
                    std::fs::write(&hook_base, head).unwrap();
                }))
            });
            let prepared = if report { repo.prepare_status_report(&line)? } else { repo.prepare_status(&line)? };
            assert!(repo.current_stage(&line)?.is_some());
            crate::fs::remove(&stage)?;
            std::fs::remove_file(&base)?;
            assert!(repo.current_stage(&line)?.is_none());
            let locked = repo.p.lock_maintenance()?;
            let st = if report {
                repo.locked_status_report(&locked, &line, Some(prepared)).context("consume report")?
            } else {
                let (st, _scan, _temp, index) = repo.locked_status(&locked, &line, Some(prepared)).context("consume snapshot")?;
                assert!(fsutil::lmeta(&index, "file").is_some(), "clean keeps the tracked file in its index");
                st
            };
            assert!(st.clean(), "transient stage must not mark a tracked file for clean");
            assert!(st.untracked.is_empty());
            assert_eq!(std::fs::read(work.join("file"))?, b"tracked");
        }
        for report in [false, true] {
            let stage = repo.p.stage_path(&line.id);
            let old = root.join("old-stage");
            crate::fs::create(&stage)?;
            std::fs::write(stage.join("file"), b"tracked")?;
            repo.mark_stage(&line)?;
            let hook_stage = stage.clone();
            let hook_old = old.clone();
            #[cfg(target_os = "macos")]
            let base_root = source.clone();
            #[cfg(target_os = "macos")]
            let base_id = line.head.clone();
            INDEX_SELECTED_HOOK.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    // Complete A was selected. reset/add replaces it with sparse B before
                    // token capture, and B's timestamps settle before preparation resumes.
                    std::fs::rename(&hook_stage, &hook_old).unwrap();
                    #[cfg(target_os = "macos")]
                    crate::thin::empty_layer(&hook_stage, &base_id, &base_root).unwrap();
                    #[cfg(not(target_os = "macos"))]
                    crate::fs::create(&hook_stage).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(1100));
                }))
            });
            let result = if report { repo.prepare_status_report(&line) } else { repo.prepare_status(&line) };
            assert!(result.is_err(), "replacement must not lend its token to the old semantics");
            #[cfg(target_os = "macos")]
            assert!(crate::thin::is_thin(&stage));
            crate::fs::remove(&stage)?;
            crate::fs::remove(&old)?;
            std::fs::remove_file(repo.stage_base_file(&line))?;
            let locked = repo.p.lock_maintenance()?;
            let st = if report {
                repo.locked_status_report(&locked, &line, None)?
            } else {
                let (st, _scan, _temp, index) = repo.locked_status(&locked, &line, None)?;
                assert!(fsutil::lmeta(&index, "file").is_some());
                st
            };
            assert!(st.clean());
            assert!(st.untracked.is_empty());
            assert_eq!(std::fs::read(work.join("file"))?, b"tracked");
        }
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let prepared = repo.prepare_status(&line)?;
        std::fs::write(work.join("file"), b"concurrent write")?;
        {
            let locked = repo.p.lock_maintenance()?;
            assert!(prepared.revalidate(&repo, &locked)?.is_none(), "a concurrent writer invalidates the capture");
        }
        std::fs::write(work.join("file"), b"tracked")?;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let prepared = repo.prepare_status(&line)?;
        std::fs::write(work.join(crate::ignore_rules::LOCAL_EXCLUDE), b"file\n")?;
        {
            let locked = repo.p.lock_maintenance()?;
            assert!(prepared.revalidate(&repo, &locked)?.is_none(), "local ignore policy belongs to the observation");
        }
        crate::fs::remove(&work).context("delete working subvolume")?;
        // An unprivileged btrfs owner must reopen read-only snapshots before removal.
        for entry in std::fs::read_dir(repo.p.scan_dir())? {
            let path = entry?.path();
            crate::fs::set_writable(&path)?;
            crate::fs::remove(&path)?;
        }
        drop(objects);
        drop(repo);
        drop(store);
        std::fs::remove_dir_all(root).context("remove fixture")?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn thin_stage_snapshot_keeps_its_selected_base_and_inherited_files() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-status-thin-index-{}", ids::new_id()));
        let store = crate::store::Store::open(&root.join("store"))?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        std::fs::create_dir_all(dir.join("meta/work"))?;
        std::fs::create_dir_all(dir.join("meta/scan"))?;
        let repo = Repo {
            p: Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))?,
            },
            ctx: None,
        };
        let head = root.join("head");
        std::fs::create_dir(&head)?;
        std::fs::write(head.join("inherited"), b"inherited")?;
        std::fs::write(head.join("edited"), b"base")?;
        let stage = root.join("stage");
        crate::thin::empty_layer(&stage, &ids::new_id(), &head)?;
        std::fs::write(stage.join("edited"), b"staged")?;
        let selected =
            SelectedIndex { source: Snap::stage(&stage, snap(&head))?, stage: Some(SelectedStage::capture(stage.clone())?) };
        let (frozen, temp) = repo.snapshot_index(&selected)?;
        // The sparse snapshot must retain its selected base independently of its marker.
        assert!(frozen.folder().is_none());
        assert_eq!(fsutil::read_file(&frozen, "inherited")?, b"inherited");
        let staged = changes::file_changes(&snap(&head), &frozen)?;
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path(), "edited");
        let work = root.join("work");
        std::fs::create_dir(&work)?;
        std::fs::write(work.join("inherited"), b"inherited")?;
        std::fs::write(work.join("edited"), b"staged")?;
        let mut st = Status::default();
        classify(&work, changes::file_changes(&frozen, &work)?, &mut st);
        assert!(st.untracked.is_empty());
        assert!(st.unstaged.is_empty());
        drop(temp);
        crate::fs::remove(&stage)?;
        drop(repo);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
