//! The merge engine. Inputs are three read-only states (base, ours, theirs) and a writable
//! snapshot of ours that receives the result. Change lists come from `btrfs send --no-data`.
//!
//! A path changed on one side takes that side (a reflink copy). A path changed on both sides
//! uses the merge driver from `.gitattributes`/`.layrattributes`: `text` (three-way line merge,
//! `git merge-file`), `union`, `ours`, `theirs`, `binary` (conflict), `blocks` (byte ranges from
//! the change lists, for formats marked safe) or a configured command. A merge never stops on a
//! conflict: the result keeps markers or the ours version and lists each conflict.

use crate::changes::{self, EntryChange};
use crate::compare::DataEvidence;
use crate::fsutil::{self, FType, Meta, Src};
use crate::model::{Conflict, ConflictKind, ConflictEntryKind};
use crate::repo::skip_path;
use crate::write_source::WriteSrc;
use anyhow::{bail, Context, Result};
use ignore::gitignore::GitignoreBuilder;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Input<'a> {
    pub base: &'a dyn Src,
    pub ours: &'a dyn Src,
    pub theirs: &'a dyn Src,
    pub result: &'a WriteSrc<'a>,
    pub base_id: &'a str,
    pub ours_id: &'a str,
    pub theirs_id: &'a str,
    pub ours_label: &'a str,
    pub theirs_label: &'a str,
    /// Project configuration: `merge.driver.<name>` and `merge.verify.<name>` commands.
    pub config: &'a dyn Fn(&str) -> Option<String>,
    /// Configured drivers run as this resolved caller, including an explicitly permitted root caller.
    pub run_as: std::sync::Arc<crate::privs::FsIdentity>,
    /// The caller is root: a driver may then run as root.
    pub allow_root: bool,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub conflicts: Vec<Conflict>,
    pub messages: Vec<String>,
    pub changed: BTreeSet<String>,
}

struct AttrRule {
    pattern: String,
    matcher: ignore::gitignore::Gitignore,
    attrs: Vec<(String, Option<String>)>,
}

pub struct Attrs {
    rules: Vec<AttrRule>,
}

impl Attrs {
    pub fn load(root: &(impl Src + ?Sized)) -> Attrs {
        let mut rules = Vec::new();
        for name in [".gitattributes", ".layrattributes"] {
            let data = match fsutil::lmeta(root, name) {
                Some(m) if m.is_file() => fsutil::read_file(root, name).unwrap_or_default(),
                _ => continue,
            };
            for line in String::from_utf8_lossy(&data).lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let pat = match parts.next() {
                    Some(p) => p,
                    None => continue,
                };
                let mut b = GitignoreBuilder::new("");
                if b.add_line(None, pat).is_err() {
                    continue;
                }
                let matcher = match b.build() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let attrs = parts
                    .map(|a| {
                        if let Some(x) = a.strip_prefix('-') {
                            (x.to_string(), Some("false".to_string()))
                        } else if let Some((k, v)) = a.split_once('=') {
                            (k.to_string(), Some(v.to_string()))
                        } else {
                            (a.to_string(), None)
                        }
                    })
                    .collect();
                rules.push(AttrRule { pattern: pat.to_string(), matcher, attrs });
            }
        }
        Attrs { rules }
    }

    /// The value of an attribute for a path: the last matching rule wins.
    pub fn get(&self, path: &str, key: &str) -> Option<String> {
        let mut out = None;
        for r in &self.rules {
            if r.matcher.matched(path, false).is_ignore() {
                for (k, v) in &r.attrs {
                    if k == key {
                        out = Some(v.clone().unwrap_or_else(|| "true".into()));
                    }
                    if k == "binary" && key == "merge" {
                        out = Some("binary".into());
                    }
                }
            }
        }
        out
    }
}

enum OursAt {
    Unchanged,
    Changed,
    Deleted,
    Moved(String),
}

struct Side {
    by_path: HashMap<String, EntryChange>,
    moved_from: HashMap<String, String>,
}

struct MergeSides {
    ours: Side,
    theirs: Side,
    ours_changes: Vec<EntryChange>,
    theirs_changes: Vec<EntryChange>,
}

impl MergeSides {
    fn new(i: &Input) -> Result<Self> {
        let (ours, ours_changes) = side(i.base, i.ours)?;
        let (theirs, theirs_changes) = side(i.base, i.theirs)?;
        Ok(Self { ours, theirs, ours_changes, theirs_changes })
    }
}

fn side(base: &dyn Src, child: &dyn Src) -> Result<(Side, Vec<EntryChange>)> {
    let mut changed = Vec::new();
    for change in changes::candidates(base, child)?.into_iter().filter(|change| !skip_path(change.path())) {
        // A full scan can include unchanged entries. Preserve adds, deletes and type changes.
        if matches!(change, EntryChange::Modify { .. }) && !changes::really_changed(base, child, &change)? {
            continue;
        }
        changed.push(change);
    }
    // Portable and full-scan candidates lack native lineage; recover moves by content.
    let list = changes::with_renames(base, child, changed);
    Ok((Side::new(&list), list))
}

impl Side {
    fn new(list: &[EntryChange]) -> Self {
        let mut by_path = HashMap::new();
        let mut moved_from = HashMap::new();
        for c in list {
            if let Some(o) = c.old_path() {
                moved_from.insert(o.clone(), c.path().clone());
            }
            by_path.insert(c.path().clone(), c.clone());
        }
        Self { by_path, moved_from }
    }
    fn at(&self, p: &str) -> OursAt {
        if let Some(q) = self.moved_from.get(p) {
            return OursAt::Moved(q.clone());
        }
        match self.by_path.get(p) {
            Some(c) if c.new().is_none() => OursAt::Deleted,
            Some(c) if c.old_path().is_none() => OursAt::Changed,
            _ => OursAt::Unchanged,
        }
    }
    fn ranges(&self, p: &str) -> DataEvidence {
        if self.moved_from.contains_key(p) {
            return DataEvidence::Unknown;
        }
        match self.by_path.get(p) {
            Some(c) if c.old_path().is_none() => c.data().clone(),
            Some(_) => DataEvidence::Unknown,
            None => DataEvidence::Equal,
        }
    }
}

struct Ctx<'a, 'b> {
    i: &'b Input<'a>,
    attrs: Attrs,
    out: Outcome,
    blocked: BTreeSet<String>,
    conflict_sides: crate::model::ConflictSides,
}

impl<'a, 'b> Ctx<'a, 'b> {
    fn conflict(&mut self, path: &str, kind: ConflictKind) {
        let label = match &kind {
            ConflictKind::Content => format!("CONFLICT (content): Merge conflict in {path}"),
            ConflictKind::AddAdd => format!("CONFLICT (add/add): Merge conflict in {path}"),
            ConflictKind::ModifyDelete => {
                format!("CONFLICT (modify/delete): {path} deleted in {} and modified in head.", self.i.theirs_label)
            }
            ConflictKind::DeleteModify => {
                format!("CONFLICT (modify/delete): {path} deleted in head and modified in {}.", self.i.theirs_label)
            }
            ConflictKind::Binary => format!("CONFLICT (binary): {path} changed on both sides; kept head"),
            k => format!("CONFLICT ({k}): {path}"),
        };
        self.out.conflicts.push(Conflict::new(path, kind, self.conflict_sides.clone()));
        self.out.messages.push(label);
    }

    fn take_theirs(&mut self, tp: &str, rp: &str) -> Result<()> {
        self.i.result.copy_from(self.i.theirs, tp, rp)?;
        self.out.changed.insert(rp.to_string());
        Ok(())
    }

    fn same(&self, ra: &(impl Src + ?Sized), pa: &str, rb: &(impl Src + ?Sized), pb: &str) -> Result<bool> {
        let (ma, mb) = (fsutil::lmeta(ra, pa), fsutil::lmeta(rb, pb));
        match (&ma, &mb) {
            (Some(a), Some(b)) if a.ftype == FType::File && a.executable() != b.executable() => Ok(false),
            _ => fsutil::same_entry(ra, pa, rb, pb, None),
        }
    }

    fn content_same(&self, ra: &(impl Src + ?Sized), pa: &str, rb: &(impl Src + ?Sized), pb: &str) -> Result<bool> {
        fsutil::same_entry(ra, pa, rb, pb, None)
    }
}

pub fn merge(i: &Input) -> Result<Outcome> {
    merge_sides(i, &MergeSides::new(i)?)
}

fn merge_sides(i: &Input, sides: &MergeSides) -> Result<Outcome> {
    let attributes = merge_tree(i, sides, true)?;
    let mut out = merge_tree(i, sides, false)?;
    out.conflicts.splice(0..0, attributes.conflicts);
    out.messages.splice(0..0, attributes.messages);
    out.changed.extend(attributes.changed);
    Ok(out)
}

fn merge_tree(i: &Input, sides: &MergeSides, attributes_only: bool) -> Result<Outcome> {
    let (ours, theirs) = (&sides.ours, &sides.theirs);
    let tlist: Vec<&EntryChange> = sides
        .theirs_changes
        .iter()
        .filter(|c| matches!(c.path().as_str(), ".gitattributes" | ".layrattributes") == attributes_only)
        .collect();
    let conflict_sides = crate::model::ConflictSides::Recorded {
        base: layr_types::StateId::parse(i.base_id)?, ours: layr_types::StateId::parse(i.ours_id)?, theirs: layr_types::StateId::parse(i.theirs_id)?,
    };
    let mut c = Ctx { i, attrs: Attrs::load(i.ours), out: Outcome::default(), blocked: BTreeSet::new(), conflict_sides };
    let derives = if attributes_only { BTreeMap::new() } else { derive_rules(i, &Attrs::load(i.result))? };
    let derived: BTreeSet<String> = derives.values().flat_map(|r| r.outputs.iter().cloned()).collect();
    let mut done: BTreeSet<String> = derived.clone();

    // 1. Moves made by theirs.
    for t in tlist.iter().filter(|t| t.is_rename()) {
        let EntryChange::Rename { old_path, path, .. } = t else { continue };
        let (src, dst) = (old_path.clone(), path.clone());
        if derived.contains(&src) || derived.contains(&dst) {
            continue;
        }
        done.insert(src.clone());
        done.insert(dst.clone());
        match ours.at(&src) {
            OursAt::Moved(q) if q != dst => {
                c.conflict(&src, ConflictKind::RenameRename { destinations: format!("{q} vs {dst}") });
            }
            OursAt::Moved(_) => {
                file_merge(&mut c, &src, &dst, &dst, ours, theirs)?;
            }
            OursAt::Deleted => {
                c.take_theirs(&dst, &dst)?;
                c.conflict(&dst, ConflictKind::RenameDelete);
            }
            OursAt::Unchanged | OursAt::Changed => {
                if fsutil::lmeta(i.result, &dst).is_some() {
                    c.conflict(&dst, ConflictKind::AddAdd);
                    continue;
                }
                i.result.make_parents(&dst)?;
                i.result.rename(&src, &dst).with_context(|| format!("move {src} to {dst}"))?;
                c.out.changed.insert(dst.clone());
                c.out.changed.insert(src.clone());
                prune(i.result, i.theirs, &src)?;
                file_merge(&mut c, &src, &dst, &dst, ours, theirs)?;
            }
        }
    }

    // 2. Adds, changes and deletes made by theirs.
    let mut dir_deletes = Vec::new();
    for t in &tlist {
        let p = t.path().clone();
        if done.contains(&p) || t.is_rename() {
            continue;
        }
        if c.blocked.iter().any(|b| p.starts_with(&format!("{b}/"))) {
            continue;
        }
        let a_b = fsutil::lmeta(i.base, &p);
        let a_t = fsutil::lmeta(i.theirs, &p);
        let q = match ours.at(&p) {
            OursAt::Moved(q) => q,
            _ => p.clone(),
        };
        let a_r = fsutil::lmeta(i.result, &q);
        // Folders.
        if a_t.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            match (&a_b, &a_r) {
                (Some(b), Some(r)) if !b.is_dir() && !r.is_dir() => {
                    if c.same(i.base, &p, i.result, &q)? {
                        i.result.remove(&q)?;
                        i.result.mkdirs(&q)?;
                        c.out.changed.insert(q.clone());
                    } else {
                        c.conflict(&p, ConflictKind::TypeChange { ours: ConflictEntryKind::File, theirs: ConflictEntryKind::Directory });
                        c.blocked.insert(p.clone());
                    }
                }
                (_, Some(r)) if !r.is_dir() => {
                    c.conflict(&p, ConflictKind::TypeChange { ours: ConflictEntryKind::File, theirs: ConflictEntryKind::Directory });
                    c.blocked.insert(p.clone());
                }
                (Some(b), None) if b.is_dir() => {
                    if dir_has_entries(i.theirs, &p) {
                        c.conflict(&p, ConflictKind::OursDeletedDirectoryWithAddedFiles);
                    }
                    i.result.mkdirs(&q)?;
                }
                (_, None) => {
                    i.result.make_parents(&format!("{q}/x"))?;
                }
                _ => {}
            }
            continue;
        }
        // Deletes.
        if a_t.is_none() {
            if a_b.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
                dir_deletes.push(p.clone());
                continue;
            }
            match ours.at(&p) {
                OursAt::Deleted => {}
                OursAt::Unchanged => {
                    if a_r.is_some() {
                        i.result.remove(&q)?;
                        c.out.changed.insert(q.clone());
                        prune(i.result, i.theirs, &q)?;
                    }
                }
                OursAt::Changed | OursAt::Moved(_) => {
                    if a_b.is_some() && a_r.is_some() && c.same(i.base, &p, i.result, &q)? {
                        i.result.remove(&q)?;
                        c.out.changed.insert(q.clone());
                    } else if a_b.is_some() {
                        c.conflict(&q, ConflictKind::ModifyDelete);
                    }
                }
            }
            continue;
        }
        // Theirs added or changed a file or a link.
        let parent = parent_of(&p);
        // A paired file move already routes this edit to its surviving parent at `q`.
        if q == p
            && !parent.is_empty()
            && fsutil::lmeta(i.base, &parent).map(|m| m.is_dir()).unwrap_or(false)
            && fsutil::lmeta(i.ours, &parent).is_none()
            && !matches!(ours.at(&parent), OursAt::Moved(_))
        {
            c.conflict(&p, ConflictKind::OursDeletedDirectoryWithChangedFile);
            c.take_theirs(&p, &p)?;
            continue;
        }
        if a_b.is_none() {
            match &a_r {
                None => c.take_theirs(&p, &q)?,
                Some(_) => {
                    if c.same(i.theirs, &p, i.result, &q)? {
                        continue;
                    }
                    add_add(&mut c, &p, &q)?;
                }
            }
            continue;
        }
        if a_r.is_none() {
            c.take_theirs(&p, &q)?;
            c.conflict(&q, ConflictKind::DeleteModify);
            continue;
        }
        file_merge(&mut c, &p, &p, &q, ours, theirs)?;
    }

    // 3. Folders deleted by theirs, deepest first.
    dir_deletes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in dir_deletes {
        if let Some(metadata) = fsutil::lmeta(i.result, &p) {
            if !metadata.is_dir() || !i.result.remove_empty_dir(&p)? {
                c.conflict(&p, ConflictKind::TheirsDeletedChangedDirectory);
            } else {
                c.out.changed.insert(p.clone());
            }
        }
    }
    if !attributes_only {
        run_derives(&mut c, derives, &sides.ours_changes, &sides.theirs_changes)?;
    }
    Ok(c.out)
}

struct DeriveRule {
    outputs: BTreeSet<String>,
    inputs: ignore::gitignore::Gitignore,
}

fn derive_rules(i: &Input, attrs: &Attrs) -> Result<BTreeMap<String, DeriveRule>> {
    if !attrs.rules.iter().any(|rule| rule.attrs.iter().any(|(key, _)| key == "derive")) {
        return Ok(BTreeMap::new());
    }
    let mut paths = BTreeSet::new();
    for root in [i.base, i.ours, i.theirs, i.result as &dyn Src] {
        fsutil::walk(root, "", &mut |p, m| {
            if skip_path(p) {
                return Ok(false);
            }
            if !m.is_dir() {
                paths.insert(p.to_string());
            }
            Ok(true)
        })?;
    }
    let mut rules: BTreeMap<String, DeriveRule> = BTreeMap::new();
    // Literal output names can be declared before the file exists on either side.
    for rule in &attrs.rules {
        if rule.attrs.iter().any(|(k, _)| k == "derive") && !rule.pattern.contains(['*', '?', '[', '!']) {
            let p = rule.pattern.trim_start_matches('/');
            if fsutil::check_rel(p).is_ok() && !p.is_empty() && !skip_path(p) {
                paths.insert(p.to_string());
            }
        }
    }
    for p in paths {
        let name = attrs.get(&p, "derive");
        let name = match name {
            Some(n) if n != "false" => n,
            _ => continue,
        };
        if !rules.contains_key(&name) {
            let mut b = GitignoreBuilder::new("");
            if let Some(inputs) = (i.config)(&format!("derive.inputs.{name}")) {
                for pattern in inputs.split_whitespace() {
                    b.add_line(None, pattern)?;
                }
            }
            rules.insert(name.clone(), DeriveRule { outputs: BTreeSet::new(), inputs: b.build()? });
        }
        rules.get_mut(&name).unwrap().outputs.insert(p);
    }
    Ok(rules)
}

fn derive_matches(rule: &DeriveRule, p: &str) -> bool {
    rule.inputs.matched_path_or_any_parents(Path::new(p), false).is_ignore()
}

/// Stable topological order. Unresolved nodes contain a cycle or depend on one.
fn derive_order(rules: &BTreeMap<String, DeriveRule>) -> (Vec<String>, BTreeSet<String>) {
    let mut pending: BTreeSet<String> = rules.keys().cloned().collect();
    let mut order = Vec::new();
    loop {
        let next = pending
            .iter()
            .find(|name| !pending.iter().any(|other| rules[other].outputs.iter().any(|p| derive_matches(&rules[*name], p))))
            .cloned();
        match next {
            Some(n) => {
                pending.remove(&n);
                order.push(n);
            }
            None => break,
        }
    }
    (order, pending)
}

fn run_derives(c: &mut Ctx, rules: BTreeMap<String, DeriveRule>, ours: &[EntryChange], theirs: &[EntryChange]) -> Result<()> {
    if rules.is_empty() {
        return Ok(());
    }
    let i = c.i;
    let mut changed = BTreeSet::new();
    for (root, list) in [(i.ours, ours), (i.theirs, theirs)] {
        for change in list {
            if changes::really_changed(i.base, root, change)? {
                changed.insert(change.path().clone());
                if let Some(old) = change.old_path() {
                    changed.insert(old.clone());
                }
            }
        }
    }
    let (order, cyclic) = derive_order(&rules);
    // Downstream rules also run when an upstream output needs regeneration.
    let mut active: BTreeSet<String> = rules
        .iter()
        .filter(|(_, rule)| changed.iter().any(|p| rule.outputs.contains(p) || derive_matches(rule, p)))
        .map(|(n, _)| n.clone())
        .collect();
    loop {
        let outputs: BTreeSet<String> = active.iter().flat_map(|n| rules[n].outputs.iter().cloned()).collect();
        let before = active.len();
        for (name, rule) in &rules {
            if outputs.iter().any(|p| derive_matches(rule, p)) {
                active.insert(name.clone());
            }
        }
        if active.len() == before {
            break;
        }
    }
    for name in cyclic {
        for p in &rules[&name].outputs {
            keep_derive_ours(i, p)?;
            c.conflict(p, ConflictKind::DeriveCycle);
        }
    }
    let mut failed = BTreeSet::new();
    for name in order {
        if !active.contains(&name) {
            continue;
        }
        let rule = &rules[&name];
        let dependency_failed = failed.iter().any(|n| rules[n].outputs.iter().any(|p| derive_matches(rule, p)));
        let result = match (i.config)(&format!("derive.command.{name}")) {
            Some(cmd) if !dependency_failed => run_derive(i, &cmd, &rule.outputs),
            _ => Ok(false),
        };
        if !result.unwrap_or(false) {
            failed.insert(name.clone());
            for p in &rule.outputs {
                keep_derive_ours(i, p)?;
                c.conflict(p, ConflictKind::DeriveFailed { name: name.clone() });
            }
        } else {
            c.out.changed.extend(rule.outputs.iter().cloned());
            c.out.messages.push(format!("Derived {name}"));
        }
    }
    Ok(())
}

fn keep_derive_ours(i: &Input, p: &str) -> Result<()> {
    if fsutil::lmeta(i.ours, p).is_some() {
        i.result.copy_from(i.ours, p, p)?;
    } else if fsutil::lmeta(i.result, p).is_some() {
        i.result.remove(p)?;
    }
    Ok(())
}

fn run_derive(i: &Input, cmd: &str, outputs: &BTreeSet<String>) -> Result<bool> {
    let dir = private_dir()?;
    let result = (|| -> Result<bool> {
        fsutil::walk(i.result, "", &mut |p, _| {
            if skip_path(p) {
                return Ok(false);
            }
            fsutil::copy_entry(i.result, p, &dir, p, None)?;
            Ok(true)
        })?;
        // The root stays private until all children have their final owner.
        if i.run_as.uid() != 0 {
            let (uid, gid) = (i.run_as.uid(), i.run_as.gid());
            fsutil::walk(&dir, "", &mut |p, _| {
                fsutil::chown_nofollow(&dir.join(p), uid, gid)?;
                Ok(true)
            })?;
        }
        hand_over(&dir, i.run_as.as_ref())?;
        if !shell_at(cmd, i.run_as.as_ref(), i.allow_root, Some(&dir))? {
            return Ok(false);
        }
        // Open every output under the user's permissions before publishing any of them.
        let opened = crate::privs::as_user_fs(i.run_as.as_ref(), || {
            let tree = crate::tree::Tree::open(&dir)?;
            outputs.iter().map(|p| Ok((p.clone(), tree.open_read(p)?))).collect::<Result<Vec<_>>>()
        });
        let files = match opened {
            Ok(files) => files,
            Err(_) => return Ok(false),
        };
        for (p, mut f) in files {
            let mode = fsutil::lmeta(i.ours, &p).map(|m| m.mode).unwrap_or(0o644);
            i.result.write_from(&p, &mut f, mode)?;
        }
        Ok(true)
    })();
    remove_private(&dir, i.run_as.as_ref());
    result
}

fn parent_of(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

fn dir_has_entries(root: &dyn Src, p: &str) -> bool {
    let mut found = false;
    fsutil::walk(root, p, &mut |_, _| {
        found = true;
        Ok(false)
    })
    .is_ok()
        && found
}

fn prune(result: &WriteSrc, theirs: &dyn Src, rel: &str) -> Result<()> {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() {
        let d = comps.join("/");
        if fsutil::lmeta(theirs, &d).is_some() {
            break;
        }
        if !result.remove_empty_dir(&d)? {
            break;
        }
        comps.pop();
    }
    Ok(())
}

fn add_add(c: &mut Ctx, p: &str, q: &str) -> Result<()> {
    let i = c.i;
    let (mr, mt) = (fsutil::lmeta(i.result, q).unwrap(), fsutil::lmeta(i.theirs, p).unwrap());
    if mr.ftype == FType::File && mt.ftype == FType::File {
        let driver = driver_for(c, q, i.result, q, i.theirs, p);
        let small = mr.size <= fsutil::MAX_TEXT && mt.size <= fsutil::MAX_TEXT;
        if (driver == "text" || driver == "union") && small {
            let empty = tempfile_path("base")?;
            std::fs::write(&empty, b"")?;
            let r = run_merge_file(i, q, &empty, "", i.theirs, p, driver == "union");
            let _ = std::fs::remove_file(&empty);
            c.out.changed.insert(q.to_string());
            c.out.messages.push(format!("Auto-merging {q}"));
            if r? > 0 {
                c.conflict(q, ConflictKind::AddAdd);
            }
            return Ok(());
        }
    }
    c.conflict(q, ConflictKind::AddAdd);
    Ok(())
}

/// Merge one entry changed by theirs: base path `bp`, theirs path `tp`, result path `rp`.
fn file_merge(c: &mut Ctx, bp: &str, tp: &str, rp: &str, ours: &Side, theirs: &Side) -> Result<()> {
    let i = c.i;
    let (mb, mt, mr) = match (fsutil::lmeta(i.base, bp), fsutil::lmeta(i.theirs, tp), fsutil::lmeta(i.result, rp)) {
        (Some(b), Some(t), Some(r)) => (b, t, r),
        (_, Some(_), None) => {
            c.take_theirs(tp, rp)?;
            return Ok(());
        }
        _ => return Ok(()),
    };
    if mr.ftype != mt.ftype {
        if mr.ftype == mb.ftype && c.content_same(i.base, bp, i.result, rp)? {
            c.take_theirs(tp, rp)?;
        } else {
            c.conflict(rp, ConflictKind::TypeChange { ours: kind(&mr), theirs: kind(&mt) });
        }
        return Ok(());
    }
    if mt.ftype == FType::Symlink {
        let (b, r, t) = (fsutil::read_link(i.base, bp)?, fsutil::read_link(i.result, rp)?, fsutil::read_link(i.theirs, tp)?);
        if t == b || t == r {
            return Ok(());
        }
        if r == b {
            c.take_theirs(tp, rp)?;
        } else {
            c.conflict(rp, ConflictKind::SymlinkTarget);
        }
        return Ok(());
    }
    if mt.ftype != FType::File {
        return Ok(());
    }
    if mb.ftype != FType::File {
        // The base was a link or a folder: both sides made a file here.
        if !c.content_same(i.result, rp, i.theirs, tp)? {
            add_add(c, tp, rp)?;
        }
        return Ok(());
    }
    // Content.
    let t_changed = !c.content_same(i.base, bp, i.theirs, tp)?;
    if t_changed {
        let o_changed = !c.content_same(i.base, bp, i.result, rp)?;
        if !o_changed {
            let mode = fsutil::lmeta(i.result, rp).unwrap().mode;
            i.result.copy_from(i.theirs, tp, rp)?;
            i.result.set_mode(rp, mode)?;
            c.out.changed.insert(rp.to_string());
        } else if !c.content_same(i.result, rp, i.theirs, tp)? {
            let tr = theirs.ranges(tp);
            let or = ours.ranges(bp);
            let driver = driver_for(c, rp, i.result, rp, i.theirs, tp);
            run_driver(c, &driver, bp, tp, rp, or, tr)?;
        }
    }
    // Executable bit, merged on its own.
    let (mb, mr, mt) = (mb, fsutil::lmeta(i.result, rp).unwrap_or(mr), mt);
    if mb.executable() != mt.executable() && mr.executable() == mb.executable() {
        let m = if mt.executable() { mr.mode | 0o111 } else { mr.mode & !0o111 };
        i.result.set_mode(rp, m)?;
        c.out.changed.insert(rp.to_string());
    } else if mb.executable() != mt.executable() && mr.executable() != mt.executable() {
        c.conflict(rp, ConflictKind::Mode);
    }
    Ok(())
}

fn kind(m: &Meta) -> ConflictEntryKind {
    match m.ftype {
        FType::File => ConflictEntryKind::File,
        FType::Dir => ConflictEntryKind::Directory,
        FType::Symlink => ConflictEntryKind::Symlink,
        FType::Special => ConflictEntryKind::Special,
    }
}

fn driver_for(c: &Ctx, attr_path: &str, ra: &(impl Src + ?Sized), pa: &str, rb: &(impl Src + ?Sized), pb: &str) -> String {
    if let Some(d) = c.attrs.get(attr_path, "merge") {
        return match d.as_str() {
            "false" => "binary".into(),
            "true" => "text".into(),
            x => x.to_string(),
        };
    }
    if fsutil::is_binary_file(ra, pa) || fsutil::is_binary_file(rb, pb) {
        "binary".into()
    } else {
        "text".into()
    }
}

fn tempfile_path(tag: &str) -> Result<PathBuf> {
    Ok(std::env::temp_dir().join(format!("layr-{tag}-{}", crate::ids::new_id())))
}

/// Copy a version into a private file, in pieces and without following links. Anything but a
/// regular file (a link, a folder, nothing) is copied as an empty file: a link in a merged tree
/// must never make the service read the file it points to.
fn export_version(source: &dyn Src, rel: &str, destination: &Path) -> Result<()> {
    if source.core_reader().is_none() {
        return copy_version(&source.at(rel)?, destination);
    }
    let meta = fsutil::lmeta(source, rel).context("tool input absent")?;
    if !meta.is_file() {
        bail!("tool input is not a regular file");
    }
    let mut input = fsutil::open_source(source, rel, &meta)?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut output = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    Ok(())
}

fn copy_version(src: &Path, dst: &Path) -> Result<()> {
    if fsutil::copy_regular(src, dst).is_err() {
        let _ = std::fs::remove_file(dst);
        std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    }
    Ok(())
}

/// A regular file too large for a text merge.
fn run_merge_file(i: &Input, rp: &str, base: &dyn Src, bp: &str, theirs: &dyn Src, tp: &str, union: bool) -> Result<i32> {
    let dir = private_dir()?;
    let (o, b, t) = (dir.join("ours"), dir.join("base"), dir.join("theirs"));
    export_version(i.result, rp, &o)?;
    export_version(base, bp, &b)?;
    export_version(theirs, tp, &t)?;
    let mut cmd = Command::new("git");
    cmd.arg("merge-file");
    if union {
        cmd.arg("--union");
    }
    cmd.args(["-L", i.ours_label, "-L", "base", "-L", i.theirs_label]).arg(&o).arg(&b).arg(&t);
    let st = cmd.status().context("run git merge-file");
    let res = match st {
        Ok(st) => match st.code() {
            Some(n) if n >= 0 => {
                // Import in place: all aliases retain the merged content and inode.
                i.result.overwrite(rp, &mut std::fs::File::open(&o)?)?;
                Ok(n)
            }
            _ => Err(anyhow::anyhow!("git merge-file failed")),
        },
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn run_driver(
    c: &mut Ctx,
    driver: &str,
    bp: &str,
    tp: &str,
    rp: &str,
    ours_ranges: DataEvidence,
    theirs_ranges: DataEvidence,
) -> Result<()> {
    let i = c.i;
    c.out.changed.insert(rp.to_string());
    match driver {
        "text" | "union" => {
            if [(i.base, bp), (i.result as &dyn Src, rp), (i.theirs, tp)]
                .iter()
                .any(|(source, path)| fsutil::lmeta(*source, path).is_none_or(|m| m.size > fsutil::MAX_TEXT))
            {
                c.conflict(rp, ConflictKind::BinaryTooLarge { limit_mib: fsutil::MAX_TEXT >> 20 });
            } else {
                c.out.messages.push(format!("Auto-merging {rp}"));
                let n = run_merge_file(i, rp, i.base, bp, i.theirs, tp, driver == "union")?;
                if n > 0 {
                    c.conflict(rp, ConflictKind::Content);
                }
            }
        }
        "ours" => {}
        "theirs" => c.take_theirs(tp, rp)?,
        "binary" => c.conflict(rp, ConflictKind::Binary),
        "blocks" => {
            c.out.messages.push(format!("Auto-merging {rp} (blocks)"));
            match block_merge(i, bp, tp, rp, ours_ranges, theirs_ranges)? {
                true => verify(c, rp)?,
                false => c.conflict(rp, ConflictKind::BinaryOverlappingBlocks),
            }
        }
        name => {
            let cmd = match (i.config)(&format!("merge.driver.{name}")) {
                Some(cmd) => cmd,
                None => {
                    c.conflict(rp, ConflictKind::UnknownMergeDriver { name: name.to_string() });
                    return Ok(());
                }
            };
            c.out.messages.push(format!("Auto-merging {rp} ({name})"));
            if !run_command_driver(i, &cmd, bp, tp, rp)? {
                c.conflict(rp, ConflictKind::DriverFailed { name: name.to_string() });
            } else {
                verify(c, rp)?;
            }
        }
    }
    Ok(())
}

fn verify(c: &mut Ctx, rp: &str) -> Result<()> {
    let i = c.i;
    let name = match c.attrs.get(rp, "merge-verify") {
        Some(n) => n,
        None => return Ok(()),
    };
    let cmd = match (i.config)(&format!("merge.verify.{name}")) {
        Some(c) => c,
        None => {
            c.conflict(rp, ConflictKind::UnknownVerifyCommand { name: name.to_string() });
            return Ok(());
        }
    };
    let dir = private_dir()?;
    let a = dir.join("result");
    export_version(i.result, rp, &a)?;
    hand_over(&dir, i.run_as.as_ref())?;
    let ok = shell_at(&cmd.replace("%A", &sh_quote(&a)).replace("%P", &sh_quote_str(rp)), i.run_as.as_ref(), i.allow_root, None);
    remove_private(&dir, i.run_as.as_ref());
    let ok = ok?;
    if !ok {
        // Keep ours: copy back the ours version.
        i.result.copy_from(i.ours, rp, rp)?;
        c.conflict(rp, ConflictKind::VerifyFailed { name: name.to_string() });
    }
    Ok(())
}

/// A new folder only root can use. The service writes the inputs of a command there, then
/// hands it over to the command's user.
fn private_dir() -> Result<PathBuf> {
    let d = tempfile_path("driver")?;
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&d)?;
    Ok(d)
}

/// Give a private folder and the files in it to the command's user: the files first, the folder
/// last, so the user cannot change the folder while the service still works in it.
fn hand_over(d: &Path, run_as: &crate::privs::FsIdentity) -> Result<()> {
    if run_as.uid() != 0 && rustix::process::geteuid().is_root() {
        for e in std::fs::read_dir(d)?.flatten() {
            fsutil::chown_nofollow(&e.path(), run_as.uid(), run_as.gid())?;
        }
        fsutil::chown_nofollow(d, run_as.uid(), run_as.gid())?;
    }
    Ok(())
}

/// Copy the file a command wrote into the result, read with the command user's file
/// permissions and without following a link: whatever the user leaves at that name, the
/// service reads only what the user can read.
/// False when there is no regular file to take.
fn take_command_output(path: &Path, run_as: &crate::privs::FsIdentity, result: &WriteSrc, rp: &str) -> Result<bool> {
    let opened = crate::privs::as_user_fs(run_as, || {
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
        if !f.metadata()?.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        Ok(f)
    });
    let mut f = match opened {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    let mode = fsutil::lmeta(result, rp).map(|m| m.mode).unwrap_or(0o644);
    result.write_from(rp, &mut f, mode)?;
    Ok(true)
}

fn remove_private(d: &Path, run_as: &crate::privs::FsIdentity) {
    let _ = crate::privs::as_user_fs(run_as, || Ok(std::fs::remove_dir_all(d)?));
}

fn sh_quote(p: &Path) -> String {
    sh_quote_str(&p.to_string_lossy())
}

fn sh_quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a configured command as the caller of the merge, with a clean environment. Never as root
/// unless the caller is root.
fn shell_at(cmd: &str, run_as: &crate::privs::FsIdentity, allow_root: bool, dir: Option<&Path>) -> Result<bool> {
    let uid = run_as.uid();
    if uid == 0 && !allow_root && rustix::process::geteuid().is_root() {
        bail!("refusing to run a merge command as root for another user");
    }
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    if let Some(dir) = dir {
        c.current_dir(dir);
    }
    run_as.command(&mut c, &[])?;
    Ok(c.status().with_context(|| format!("run {cmd}"))?.success())
}

/// A configured driver command, as in git: `%O` base, `%A` ours (the result is written here),
/// `%B` theirs, `%P` path, `%L` marker size.
fn run_command_driver(i: &Input, cmd: &str, bp: &str, tp: &str, rp: &str) -> Result<bool> {
    let dir = private_dir()?;
    let (o, a, bb) = (dir.join("base"), dir.join("ours"), dir.join("theirs"));
    export_version(i.base, bp, &o)?;
    export_version(i.result, rp, &a)?;
    export_version(i.theirs, tp, &bb)?;
    hand_over(&dir, i.run_as.as_ref())?;
    let full = cmd
        .replace("%O", &sh_quote(&o))
        .replace("%A", &sh_quote(&a))
        .replace("%B", &sh_quote(&bb))
        .replace("%P", &sh_quote_str(rp))
        .replace("%L", "7");
    let ran = shell_at(&full, i.run_as.as_ref(), i.allow_root, None);
    // A result the service cannot read as the driver user (a link, a folder) is a failed driver.
    let taken = match ran {
        Ok(true) => take_command_output(&a, i.run_as.as_ref(), i.result, rp),
        _ => Ok(false),
    };
    remove_private(&dir, i.run_as.as_ref());
    ran?;
    taken
}

fn overlaps(a: &[(u64, u64)], b: &[(u64, u64)]) -> bool {
    for (ao, al) in a {
        let ae = ao.saturating_add(*al);
        for (bo, bl) in b {
            let be = bo.saturating_add(*bl);
            if *ao < be && *bo < ae {
                return true;
            }
        }
    }
    false
}

/// Compare fixed blocks when portable states have no native extent evidence. Adjacent
/// changed blocks coalesce; reading bounded buffers also supports large binary files.
fn changed_blocks(
    base: &(impl Src + ?Sized),
    bp: &str,
    child: &(impl Src + ?Sized),
    cp: &str,
    size: u64,
) -> Result<DataEvidence> {
    use std::io::Read;
    let ma = fsutil::lmeta(base, bp).context("base entry absent")?;
    let mb = fsutil::lmeta(child, cp).context("child entry absent")?;
    let (mut a, mut b) = (fsutil::open_source(base, bp, &ma)?, fsutil::open_source(child, cp, &mb)?);
    let (mut ab, mut bb) = ([0; 4096], [0; 4096]);
    let mut ranges: Vec<(u64, crate::compare::RangeEnd)> = Vec::new();
    let mut pos = 0;
    while pos < size {
        let len = usize::try_from((size - pos).min(ab.len() as u64))?;
        a.read_exact(&mut ab[..len])?;
        b.read_exact(&mut bb[..len])?;
        let end = pos + len as u64;
        if ab[..len] != bb[..len] {
            match ranges.last_mut() {
                Some((_, previous)) if *previous == crate::compare::RangeEnd::Offset(pos) => {
                    *previous = crate::compare::RangeEnd::Offset(end);
                }
                _ => ranges.push((pos, crate::compare::RangeEnd::Offset(end))),
            }
        }
        pos = end;
    }
    Ok(DataEvidence::from_ranges(ranges.try_into().map_err(anyhow::Error::msg)?, size, size))
}

/// Merge equal-sized binary files when their changed blocks do not overlap. Native
/// comparisons supply extent evidence; object-backed comparisons read blocks on demand.
fn block_merge(
    i: &Input,
    bp: &str,
    tp: &str,
    rp: &str,
    mut ours_ranges: DataEvidence,
    mut theirs_ranges: DataEvidence,
) -> Result<bool> {
    let (mb, mo, mt) = match (fsutil::lmeta(i.base, bp), fsutil::lmeta(i.result, rp), fsutil::lmeta(i.theirs, tp)) {
        (Some(b), Some(o), Some(t)) if b.is_file() && o.is_file() && t.is_file() => (b, o, t),
        _ => return Ok(false),
    };
    if mb.size != mo.size || mb.size != mt.size {
        return Ok(false);
    }
    if matches!(ours_ranges, DataEvidence::Unknown)
        && (i.base.objects().is_some()
            || i.ours.objects().is_some()
            || i.base.core_reader().is_some()
            || i.ours.core_reader().is_some())
    {
        ours_ranges = changed_blocks(i.base, bp, i.result, rp, mb.size)?;
    }
    if matches!(theirs_ranges, DataEvidence::Unknown)
        && (i.base.objects().is_some()
            || i.theirs.objects().is_some()
            || i.base.core_reader().is_some()
            || i.theirs.core_reader().is_some())
    {
        theirs_ranges = changed_blocks(i.base, bp, i.theirs, tp, mb.size)?;
    }
    if matches!(ours_ranges, DataEvidence::Unknown) || matches!(theirs_ranges, DataEvidence::Unknown) {
        return Ok(false);
    }
    let bounded = |data: DataEvidence| match data {
        DataEvidence::ChangedRanges(ranges) => ranges.bounded(mb.size),
        _ => Vec::new(),
    };
    let (or, tr) = (bounded(ours_ranges), bounded(theirs_ranges));
    if overlaps(&or, &tr) {
        return Ok(false);
    }
    let native_source = if i.theirs.core_reader().is_none() { Some(fsutil::open_regular(&i.theirs.at(tp)?)?) } else { None };
    let mut source = fsutil::open_source(i.theirs, tp, &mt)?;
    i.result.patch(rp, source.as_mut(), native_source.as_ref(), &tr)?;
    Ok(true)
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    #[test]
    fn fallback_does_not_resurrect_an_unchanged_file_deleted_by_ours() -> Result<()> {
        let root = tempfile_path("fallback-deletion")?;
        for dir in ["base", "ours", "theirs", "result"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        let (base, ours, theirs, result) = (root.join("base"), root.join("ours"), root.join("theirs"), root.join("result"));
        std::fs::write(base.join("file"), b"unchanged")?;
        std::fs::write(theirs.join("file"), b"unchanged")?;
        std::fs::write(theirs.join("added"), b"new")?;
        // Ordinary directories cannot provide native send/layer hints: this uses fallback.
        let i = Input {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            result: &WriteSrc::Native(&result),
            base_id: "01a12290-0000-7000-8000-000000000001",
            ours_id: "01a12290-0000-7000-8000-000000000002",
            theirs_id: "01a12290-0000-7000-8000-000000000003",
            ours_label: "ours",
            theirs_label: "theirs",
            config: &|_| None,
            run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
            allow_root: false,
        };
        let outcome = merge(&i)?;
        assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
        assert!(!result.join("file").exists());
        assert_eq!(std::fs::read(result.join("added"))?, b"new");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn block_merge_bounds_eof_and_rejects_unknown_overlap_and_resize() -> Result<()> {
        let root = tempfile_path("block-evidence")?;
        for dir in ["base", "ours", "theirs", "result"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        let (base, ours, theirs, result) = (root.join("base"), root.join("ours"), root.join("theirs"), root.join("result"));
        std::fs::write(base.join("file"), b"0000000000000000")?;
        std::fs::write(ours.join("file"), b"ours000000000000")?;
        std::fs::write(result.join("file"), b"ours000000000000")?;
        std::fs::write(theirs.join("file"), b"00000000theirs00")?;
        let i = Input {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            result: &WriteSrc::Native(&result),
            base_id: "base",
            ours_id: "ours",
            theirs_id: "theirs",
            ours_label: "ours",
            theirs_label: "theirs",
            config: &|_| None,
            run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
            allow_root: false,
        };
        let ranges = |start, len| DataEvidence::from_native(&[(start, len)], 16, 16).unwrap();
        assert!(!block_merge(&i, "file", "file", "file", DataEvidence::Unknown, ranges(8, u64::MAX))?);
        assert!(!block_merge(&i, "file", "file", "file", ranges(0, 10), ranges(8, u64::MAX))?);
        assert!(block_merge(&i, "file", "file", "file", ranges(0, 4), ranges(8, u64::MAX))?);
        assert_eq!(std::fs::read(result.join("file"))?, b"ours0000theirs00");
        std::fs::write(theirs.join("file"), b"short")?;
        assert!(!block_merge(&i, "file", "file", "file", DataEvidence::Equal, DataEvidence::Equal)?);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[cfg(test)]
mod derive_tests {
    use super::*;

    fn rule(output: &str, inputs: &str) -> DeriveRule {
        let mut b = GitignoreBuilder::new("/tree");
        for pattern in inputs.split_whitespace() {
            b.add_line(None, pattern).unwrap();
        }
        DeriveRule { outputs: BTreeSet::from([output.into()]), inputs: b.build().unwrap() }
    }

    #[test]
    fn derive_dependencies_override_name_order() {
        let rules = BTreeMap::from([
            ("a".into(), rule("final", "middle")),
            ("z".into(), rule("middle", "src/**")),
            ("b".into(), rule("independent", "other")),
        ]);
        let (order, cycles) = derive_order(&rules);
        assert_eq!(order, ["b", "z", "a"]);
        assert!(cycles.is_empty());
        assert!(derive_matches(&rules["z"], "src/deep/input"));
    }

    #[test]
    fn derive_cycles_block_dependents() {
        let rules = BTreeMap::from([
            ("a".into(), rule("first", "second")),
            ("b".into(), rule("second", "first")),
            ("c".into(), rule("last", "second")),
            ("d".into(), rule("independent", "source")),
        ]);
        let (order, cycles) = derive_order(&rules);
        assert_eq!(order, ["d"]);
        assert_eq!(cycles, BTreeSet::from(["a".into(), "b".into(), "c".into()]));
        let rules = BTreeMap::from([("self".into(), rule("output", "output"))]);
        assert!(!derive_order(&rules).1.is_empty());
    }

    #[test]
    fn derive_takes_only_declared_outputs_and_rejects_links() -> Result<()> {
        let root = private_dir()?;
        let result = (|| -> Result<()> {
            for sub in ["base", "ours", "theirs", "result"] {
                std::fs::create_dir(root.join(sub))?;
            }
            let result = root.join("result");
            std::fs::write(result.join("input"), b"source")?;
            std::fs::write(result.join("output"), b"ours")?;
            let uid = rustix::process::geteuid().as_raw();
            let gid = rustix::process::getegid().as_raw();
            let i = Input {
                base: &root.join("base"),
                ours: &root.join("ours"),
                theirs: &root.join("theirs"),
                result: &WriteSrc::Native(&result),
                base_id: "01a12290-0000-7000-8000-000000000001",
                ours_id: "01a12290-0000-7000-8000-000000000002",
                theirs_id: "01a12290-0000-7000-8000-000000000003",
                ours_label: "ours",
                theirs_label: "theirs",
                config: &|_| None,
                run_as: std::sync::Arc::new(crate::privs::FsIdentity::resolve(uid, gid)?),
                allow_root: uid == 0,
            };
            let outputs = BTreeSet::from(["output".into()]);
            assert!(run_derive(&i, "cat input > output; echo changed > input", &outputs)?);
            assert_eq!(std::fs::read(result.join("output"))?, b"source");
            assert_eq!(std::fs::read(result.join("input"))?, b"source");
            assert!(!run_derive(&i, "rm output; ln -s /etc/passwd output", &outputs)?);
            assert_eq!(std::fs::read(result.join("output"))?, b"source");
            assert!(!run_derive(&i, "echo bad > output; exit 1", &outputs)?);
            let nested = BTreeSet::from(["nested/passwd".into()]);
            assert!(!run_derive(&i, "ln -s /etc nested", &nested)?);
            assert_eq!(std::fs::read(result.join("output"))?, b"source");
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }
}

#[cfg(test)]
mod merge_sides_tests {
    use super::*;

    #[test]
    fn shared_sides_preserve_attributes_derives_renames_and_conflict_order() -> Result<()> {
        let root = private_dir()?;
        let result = (|| -> Result<()> {
            for sub in ["base", "ours", "theirs", "result"] {
                std::fs::create_dir(root.join(sub))?;
            }
            let base = root.join("base");
            let ours = root.join("ours");
            let theirs = root.join("theirs");
            let result = root.join("result");
            let attrs = ".gitattributes merge=binary\nz merge=binary\n";
            let uid = rustix::process::geteuid().as_raw();
            for (dir, label) in [(&base, "base"), (&ours, "ours"), (&theirs, "theirs")] {
                std::fs::write(dir.join(".gitattributes"), label)?;
                std::fs::write(dir.join(".layrattributes"), attrs)?;
                std::fs::write(dir.join("z"), label)?;
                std::fs::write(dir.join("source"), label)?;
            }
            std::fs::write(base.join("original"), "moved")?;
            std::fs::write(theirs.join("moved"), "moved")?;
            std::fs::write(theirs.join(".layrattributes"), format!("{attrs}generated derive=build\n"))?;
            for name in [".gitattributes", ".layrattributes", "z", "source"] {
                std::fs::copy(ours.join(name), result.join(name))?;
            }
            let change = |path: &str, old_path: Option<&str>, child: &Path| {
                if let Some(src) = old_path {
                    EntryChange::Rename {
                        path: path.into(),
                        old_path: src.into(),
                        old: fsutil::lmeta(&base, src).unwrap(),
                        new: fsutil::lmeta(child, path).unwrap(),
                        data: DataEvidence::Unknown,
                    }
                } else {
                    EntryChange::observed(
                        path.into(),
                        fsutil::lmeta(&base, path),
                        fsutil::lmeta(child, path),
                        DataEvidence::Unknown,
                    )
                    .unwrap()
                }
            };
            let ours_changes = vec![
                change(".gitattributes", None, &ours),
                change("original", None, &ours),
                change("source", None, &ours),
                change("z", None, &ours),
            ];
            let theirs_changes = vec![
                change(".gitattributes", None, &theirs),
                change(".layrattributes", None, &theirs),
                change("moved", Some("original"), &theirs),
                change("source", None, &theirs),
                change("z", None, &theirs),
            ];
            let sides =
                MergeSides { ours: Side::new(&ours_changes), theirs: Side::new(&theirs_changes), ours_changes, theirs_changes };
            let config = |key: &str| match key {
                "derive.command.build" => Some("cat source > generated".into()),
                "derive.inputs.build" => Some("source".into()),
                _ => None,
            };
            let i = Input {
                base: &base,
                ours: &ours,
                theirs: &theirs,
                result: &WriteSrc::Native(&result),
                base_id: "01a12290-0000-7000-8000-000000000001",
                ours_id: "01a12290-0000-7000-8000-000000000002",
                theirs_id: "01a12290-0000-7000-8000-000000000003",
                ours_label: "ours",
                theirs_label: "theirs",
                config: &config,
                run_as: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()).fs_identity()?,
                // The privileged checks run this as root, where a derive needs the explicit opt-in.
                allow_root: rustix::process::geteuid().is_root(),
            };
            let out = merge_sides(&i, &sides)?;
            assert_eq!(std::fs::read(result.join(".gitattributes"))?, b"ours");
            assert_eq!(std::fs::read(result.join(".layrattributes"))?, std::fs::read(theirs.join(".layrattributes"))?);
            assert_eq!(std::fs::read(result.join("moved"))?, b"moved");
            assert_eq!(std::fs::read(result.join("z"))?, b"ours");
            // git merge-file adds a base part with the user's merge.conflictStyle diff3 or zdiff3.
            let source = std::fs::read_to_string(result.join("source"))?;
            assert!(
                source.starts_with("<<<<<<< ours\nours\n") && source.ends_with("=======\ntheirs\n>>>>>>> theirs\n"),
                "{source}"
            );
            assert_eq!(std::fs::read(result.join("generated"))?, std::fs::read(result.join("source"))?);
            assert_eq!(
                out.conflicts.iter().map(|c| (c.path.as_str(), c.kind())).collect::<Vec<_>>(),
                [(".gitattributes", "binary"), ("moved", "rename/delete"), ("source", "content"), ("z", "binary")]
                    .map(|(path, kind)| (path, std::borrow::Cow::Borrowed(kind)))
            );
            assert_eq!(
                out.messages,
                [
                    "CONFLICT (binary): .gitattributes changed on both sides; kept head",
                    "CONFLICT (rename/delete): moved",
                    "Auto-merging source",
                    "CONFLICT (content): Merge conflict in source",
                    "CONFLICT (binary): z changed on both sides; kept head",
                    "Derived build",
                ]
            );
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }
}

#[cfg(test)]
mod sides_measurements {
    use super::*;

    fn sides(reuse: bool) {
        let changes: Vec<_> = (0..100_000)
            .map(|n| EntryChange::Add {
                path: format!("folder/file-{n}"),
                new: Meta { ftype: FType::File, mode: 0o644, size: 4096, ino: 0, uid: 0, gid: 0, mtime: 0 },
            })
            .collect();
        let build = || (Side::new(&changes), changes.clone());
        if reuse {
            let (ours, ours_changes) = build();
            let (theirs, theirs_changes) = build();
            let sides = MergeSides { ours, theirs, ours_changes, theirs_changes };
            for _ in [true, false] {
                std::hint::black_box(&sides);
            }
        } else {
            for attributes_only in [true, false] {
                let ours = build();
                let theirs = build();
                std::hint::black_box((&ours, &theirs));
                if !attributes_only {
                    let (_, full_theirs) = build();
                    std::hint::black_box(&full_theirs);
                }
            }
        }
    }

    #[test]
    #[ignore = "candidate-map measurement; excludes filesystem candidate discovery"]
    fn merge_sides_recomputed() {
        sides(false)
    }

    #[test]
    #[ignore = "candidate-map measurement; excludes filesystem candidate discovery"]
    fn merge_sides_shared() {
        sides(true)
    }
}

#[cfg(test)]
mod command_identity_tests {
    use super::*;

    #[test]
    fn merge_commands_reuse_one_identity_for_root_and_nonroot_callers() -> Result<()> {
        let uid = rustix::process::geteuid().as_raw();
        if crate::test_runs_as_root() {
            return Ok(());
        }
        for target in [uid, 0] {
            let caller = crate::ctx::Caller::from_uid(target);
            let before = crate::privs::IDENTITY_LOOKUPS.with(|lookups| lookups.get());
            for _ in 0..4 {
                let identity = caller.fs_identity()?;
                let result = shell_at("true", &identity, caller.is_root(), None);
                if caller.is_root() {
                    // Exercise root command preparation without ever starting a root process.
                    let error = result.err().ok_or_else(|| anyhow::anyhow!("root command unexpectedly succeeded"))?;
                    assert!(error.to_string().contains("cannot run a command as uid 0"));
                } else {
                    assert!(result?);
                }
            }
            assert_eq!(crate::privs::IDENTITY_LOOKUPS.with(|lookups| lookups.get()) - before, 1);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "merge_source_tests.rs"]
mod source_tests;
