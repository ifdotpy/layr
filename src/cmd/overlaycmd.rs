//! `layr overlay`: file trees with an owner, readers and their own history, shown in lines above
//! the shared set (docs/overlays.md; the model is in src/overlay.rs).

use super::{exit, here};
use crate::access::{parse_principal, show_principal, Policy, Role};
use crate::args::{parse, Spec};
use crate::cache_overlay::FolderIn;
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, StateKind, StateRec};
use crate::overlay::{Layer, MergeKind, Mode, Props, PROPS};
use crate::repo::Repo;
use crate::storage as btrfs;
use crate::store::{check_name, Project};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

const USAGE: &str =
    "usage: layr overlay create <name> [--readers <principal>,...] [--mode shared|follow] [--hidden] [--folder <path>] [<properties>]
       layr overlay set <name> <properties>
       layr overlay remote add <name> <remote> <git url> | list <name> | remove <name> <remote>
       layr overlay show-to-project <name>
       layr overlay list [--json] [--size]
       layr overlay readers <name> [<principal>...]
       layr overlay managers <name> [machine:<id>...]
       layr overlay use [--add|--remove] <name>... | --none
       layr overlay order [<name>,...]
       layr overlay move [--purge] <name> <path>... | --cancel [<path>...]
       layr overlay repair <overlay>
       layr overlay remove <cache overlay>
       layr overlay show <name> --json
       layr overlay log <name> --ops [--json]
       layr overlay log|show|diff|grep|ls-files|blame <name> [<args>]
       layr overlay migrate-parts [-n|--dry-run] [-q]
properties: --history all|last|<N>  --quiet|--no-quiet  --merge three-way|union-theirs  --gc none|lru:<size>  --key-files <files>|none
            --platform <key>|any  --trust protected|readers";

pub fn overlay(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]).to_vec();
    match sub {
        "create" => create(ctx, &rest),
        "set" => set(ctx, &rest),
        "remote" => super::layerpush::overlay_remote(ctx, &rest),
        "list" => list(ctx, &rest),
        "show-to-project" => show_to_project(ctx, &rest),
        "readers" => readers(ctx, &rest),
        "managers" => managers(ctx, &rest),
        "mode" => access_mode(ctx, &rest),
        "use" => use_cmd(ctx, &rest),
        "order" => order(ctx, &rest),
        "move" => move_cmd(ctx, &rest),
        "repair" => repair(ctx, &rest),
        "remove" => remove(ctx, &rest),
        "migrate-parts" => migrate_parts(ctx, &rest),
        "show" if rest.iter().take_while(|s| s.as_str() != "--").any(|s| s == "--json") => show_json(ctx, &rest),
        "log" if rest.iter().take_while(|s| s.as_str() != "--").any(|s| s == "--ops") => operations(ctx, &rest),
        "log" | "show" | "diff" | "grep" | "ls-files" | "blame" | "rev-parse" => read(ctx, sub, &rest),
        _ => Err(exit(128, USAGE)),
    }
}

/// Principals as typed (`alice`, `@devs`, `uid:1001`, comma separated lists allowed).
fn principals(list: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list.iter().flat_map(|s| s.split(',')).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Some(machine) = item.strip_prefix("machine:") {
            if !crate::model::is_uuid(machine) {
                return Err(exit(128, "a machine reader needs its UUID"));
            }
            out.push(item.to_string());
            continue;
        }
        if let Some(person) = item.strip_prefix("person:") {
            let (login, recipient) = person.split_once('=').ok_or_else(|| exit(128, "use person:<login>=<age recipient>"))?;
            if !layr_store::person::login_name(login) || !layr_store::keys::valid_recipient(recipient) {
                return Err(exit(128, "invalid person reader or recipient"));
            }
            out.push(item.to_string());
            continue;
        }
        match parse_principal(item) {
            Some(p) => out.push(p),
            None => return Err(exit(128, format!("fatal: unknown user or group '{item}'"))),
        }
    }
    Ok(out)
}

/// The access policy of an overlay: its owner is its admin, the readers read it.
pub(crate) fn access_map(owner: u32, readers: &[String]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for r in readers {
        m.insert(r.clone(), serde_json::json!("reader"));
    }
    m.insert(format!("user:{owner}"), serde_json::json!("admin"));
    serde_json::Value::Object(m)
}

fn create(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = props_spec(Spec::new().value("--readers|-r").value("--mode").flag("--hidden").flag("-q").value("--folder"));
    let a = parse(&spec, args)?;
    let props = props_of(&a, Props::default())?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if repo.p.is_overlay() {
        return Err(exit(1, "error: an overlay has no overlays"));
    }
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    check_name("overlay", &name)?;
    repo.require(ctx, "line.create")?;
    let hidden = a.has("--hidden");
    let folder = a.get("--folder").map(crate::overlay::folder_path).transpose()?;
    // A line has its own part on each overlay, and a new line inherits the part of its source (owner decision
    // 2026-11-11): the shared mode (one overlay line for all lines of a user) is gone for new overlays. Old shared
    // overlays keep working.
    let mode = match a.get("--mode") {
        Some("shared") => {
            return Err(exit(
                2,
                "layr overlay create --mode shared was removed: each line has its part of an overlay and a new line inherits it. Leave out --mode",
            ))
        }
        Some(m) => Mode::parse(m).ok_or_else(|| exit(128, format!("fatal: unknown mode '{m}' (follow)")))?,
        None => Mode::Follow,
    };
    if hidden && mode != Mode::Follow {
        return Err(exit(128, "hidden overlays require follow mode"));
    }
    if folder.is_some() && (hidden || mode != Mode::Follow) {
        return Err(exit(128, "a folder overlay uses follow mode and is not hidden"));
    }
    let readers = principals(&a.all("--readers"))?;
    let _lock = repo.p.lock()?;
    if repo.overlay_info(&name)?.is_some() {
        return Err(exit(128, format!("fatal: overlay '{name}' already exists")));
    }
    // One folder overlay per folder and platform: build output of each platform is its own overlay.
    if let Some(f) = &folder {
        let clash = |p: &Option<String>| p.is_none() || props.platform.is_none() || *p == props.platform;
        if let Some((other, _, _)) = repo.all_folder_overlays()?.into_iter().find(|(_, x, p)| x == f && clash(p)) {
            return Err(exit(128, format!("fatal: overlay '{other}' already holds folder {f}")));
        }
    }
    let key_files = a.get("--key-files").map(key_files_value).transpose()?;
    make_overlay(ctx, repo, &name, &readers, mode, hidden, &props, folder.as_deref(), key_files.as_ref())?;
    if !a.has("-q") {
        let mut who: Vec<String> = vec![ctx.caller.user.clone()];
        who.extend(readers.iter().map(|r| show_principal(r)));
        who.dedup();
        let mut shown = props.describe();
        if let Some(f) = &folder {
            shown.insert(0, format!("folder={f}"));
        }
        let extra = if shown.is_empty() { String::new() } else { format!("; {}", shown.join(" ")) };
        outln!(ctx, "Created overlay {name} (mode {}; readers: {}{extra})", mode.name(), who.join(", "));
    }
    Ok(0)
}

/// Make overlay `name` of the caller with its empty first state and its records. The caller holds
/// the project lock and has checked that no overlay has the name. `folder`: a folder overlay;
/// `cache`: the cache overlay of a cache path (`save --overlays`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_overlay(
    ctx: &Ctx,
    repo: &Repo,
    name: &str,
    readers: &[String],
    mode: Mode,
    hidden: bool,
    props: &Props,
    folder: Option<&str>,
    key_files: Option<&serde_json::Value>,
) -> Result<()> {
    let owner = ctx.caller.uid;
    let pname = format!("{}%{name}", repo.p.name);
    let dir = ctx.store.project_dir(&pname)?;
    // A removed cache overlay of this name keeps its folder until gc frees its states; the new
    // overlay replaces that folder (its states are not kept by any line).
    let removed = repo.p.db.overlay_config(&format!("overlay.{name}.removed"))?.is_some();
    if removed && dir.exists() {
        btrfs::delete_tree(&dir)?;
    }
    // Only a folder this call made is removed below, so the creation stays outside the cleanup.
    let config = serde_json::json!({
        "access": access_map(owner, readers),
        "overlay": {"project": repo.p.id, "name": name, "mode": mode.name(), "hidden": hidden},
    });
    let op = Project::create_overlay(&ctx.store, &pname, &ctx.actor(), config)?;
    let made = (|| -> Result<String> {
        let orepo = Repo { p: op, ctx: Some(ctx) };
        // The first state of every overlay line: an empty tree.
        let id = ids::new_id();
        let dst = orepo.p.state_path(&id);
        btrfs::create_subvolume(&dst)?;
        btrfs::set_readonly(&dst)?;
        let info = btrfs::subvol_info(&dst)?;
        let st = StateRec {
            id: id.clone(),
            kind: StateKind::Commit,
            change_id: Some(ids::new_id()),
            derived_from: None,
            context: None,
            parents: Vec::new(),
            line: None,
            subvol: info.uuid,
            author: ctx.author(),
            message: format!("overlay {name}"),
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
        };
        let mut rec = Record::new("commit");
        rec.states.push(st);
        orepo.commit_record(ctx, rec)?;
        let mut rec = Record::new("overlay.create");
        let mut cfg = serde_json::Map::new();
        cfg.insert(
            format!("overlay.{name}"),
            serde_json::json!({"id":orepo.p.id,"owner": owner, "mode": mode.name(), "root": id, "hidden": hidden}),
        );
        // Properties are separate settings, so an older layr reads the overlay as before.
        for prop in PROPS {
            let v = props.value(prop);
            if !v.is_null() {
                cfg.insert(format!("overlay.{name}.{prop}"), v);
            }
        }
        if let Some(f) = folder {
            cfg.insert(format!("overlay.{name}.folder"), serde_json::json!(f));
        }
        if let Some(k) = key_files {
            cfg.insert(format!("overlay.{name}.key-files"), k.clone());
        }
        if removed {
            cfg.insert(format!("overlay.{name}.removed"), serde_json::Value::Null);
        }
        rec.data = serde_json::json!({"name": name, "parent":repo.p.id, "config": cfg});
        if hidden {
            orepo.commit_record(ctx, rec)?;
        } else {
            repo.commit_record(ctx, rec)?;
        }
        Ok(id)
    })();
    if let Err(e) = made {
        // No record of the project names the overlay: its folder goes.
        if repo.overlay_info(name)?.is_none() {
            let _ = btrfs::delete_tree(&dir);
        }
        return Err(e);
    }
    Ok(())
}

/// A folder overlay is not part of the file stack: `save -m`, `line new` and `merge` move it (docs/cache-overlays.md, section 3).
fn refuse_folder(repo: &Repo, name: &str) -> Result<()> {
    if repo.overlay_info(name)?.is_some() {
        if let Some(f) = repo.overlay_setting(name, "folder")? {
            return Err(exit(
                1,
                format!(
                    "error: overlay '{name}' is the folder overlay of {}; use layr save -m or layr merge",
                    f.as_str().unwrap_or("?")
                ),
            ));
        }
    }
    Ok(())
}

/// The options of overlay properties (docs/cache-overlays.md, section 4).
fn props_spec(spec: Spec) -> Spec {
    spec.value("--history")
        .flag("--quiet")
        .flag("--no-quiet")
        .value("--merge")
        .value("--gc")
        .value("--key-files")
        .value("--prune")
        .value("--platform")
        .value("--trust")
}

/// `--key-files a,b` as the setting value; `none` clears it.
fn key_files_value(v: &str) -> Result<serde_json::Value> {
    if v == "none" {
        return Ok(serde_json::Value::Null);
    }
    let mut files = Vec::new();
    for f in v.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        files.push(crate::overlay::folder_path(f)?);
    }
    Ok(serde_json::json!(files))
}

/// `base` with the property options of `a`.
fn props_of(a: &crate::args::Parsed, base: Props) -> Result<Props> {
    let mut p = base;
    if let Some(v) = a.get("--history") {
        p.history = crate::overlay::parse_history(v)
            .ok_or_else(|| exit(128, format!("fatal: invalid --history '{v}' (all, last or a number)")))?;
    }
    if a.has("--quiet") && a.has("--no-quiet") {
        return Err(exit(128, "fatal: --quiet and --no-quiet exclude each other"));
    }
    if a.has("--quiet") {
        p.quiet = true;
    }
    if a.has("--no-quiet") {
        p.quiet = false;
    }
    if let Some(v) = a.get("--merge") {
        p.merge =
            MergeKind::parse(v).ok_or_else(|| exit(128, format!("fatal: invalid --merge '{v}' (three-way or union-theirs)")))?;
    }
    if let Some(v) = a.get("--prune") {
        // Core checks the form only. The adapter names come from the registry, and an unknown one is logged at prune time.
        p.prune = match v {
            "auto" => None,
            _ => Some(
                crate::overlay::prune_name(v)
                    .ok_or_else(|| exit(128, format!("fatal: invalid --prune '{v}' (an adapter name, none or auto)")))?,
            ),
        };
    }
    if let Some(v) = a.get("--platform") {
        // `--platform any` clears it: the overlay is on every platform.
        p.platform = match v {
            "any" => None,
            _ => Some(crate::overlay::platform_name(v).ok_or_else(|| {
                exit(128, format!("fatal: invalid --platform '{v}' (a key such as {}, or any)", crate::model::cache_platform()))
            })?),
        };
    }
    if let Some(v) = a.get("--trust") {
        p.trust = Some(
            crate::overlay::Trust::parse(v)
                .ok_or_else(|| exit(128, format!("fatal: invalid --trust '{v}' (protected or readers)")))?,
        );
    }
    if let Some(v) = a.get("--gc") {
        p.gc_lru = crate::overlay::parse_gc(v)
            .ok_or_else(|| exit(128, format!("fatal: invalid --gc '{v}' (none or lru:<size>, for example lru:20G)")))?;
    }
    Ok(p)
}

/// `layr overlay set <name> <properties>`: the owner changes properties. The settings go into the
/// records of the project, or of the overlay when it is hidden.
fn set(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&props_spec(Spec::new()), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    if a.pos.len() != 1 {
        return Err(exit(128, USAGE));
    }
    let info = visible_info(ctx, &h.repo, &name)?;
    let orepo = h.repo.overlay_repo(&name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, format!("permission denied: only the owner of overlay '{name}' sets its properties")));
    }
    let _lock = h.repo.p.lock()?;
    let old = h.repo.overlay_props(&name)?;
    let new = props_of(&a, old.clone())?;
    let mut cfg = serde_json::Map::new();
    for prop in PROPS {
        if old.value(prop) != new.value(prop) {
            cfg.insert(format!("overlay.{name}.{prop}"), new.value(prop));
        }
    }
    // The files of the platform key of a cache (docs/cache-overlays.md, section 6).
    if let Some(v) = a.get("--key-files") {
        cfg.insert(format!("overlay.{name}.key-files"), key_files_value(v)?);
    }
    if !cfg.is_empty() {
        let mut rec = Record::new("config");
        rec.data = serde_json::json!({"config": cfg});
        if info.hidden {
            let _olock = orepo.p.lock()?;
            orepo.commit_record(ctx, rec)?;
        } else {
            h.repo.commit_record(ctx, rec)?;
        }
    }
    let shown = new.describe();
    outln!(ctx, "overlay {name}: {}", if shown.is_empty() { "no properties".to_string() } else { shown.join(" ") });
    if new.history.is_some() && new.history != old.history {
        h.repo.after_overlay_move(ctx, &name);
    }
    Ok(0)
}

/// The principals with the reader role (or more) of an overlay.
fn reader_list(p: &Project) -> Vec<String> {
    let pol = Policy::load(p);
    pol.access.iter().filter(|(_, r)| **r >= Role::Reader).map(|(k, _)| show_principal(k)).collect()
}

/// A folder overlay's folder in a line, as `cache list -v` showed it: the state, the size of a
/// cache, and the head of the overlay line that the line saved.
struct InLine {
    state: String,
    size: Option<u64>,
    saved: Option<String>,
}

/// Machine-readable listing; an unmeasured cache size is absent.
#[derive(serde::Serialize)]
struct ListingRow {
    name: String,
    mode: String,
    hidden: bool,
    owner: u32,
    readers: Vec<String>,
    shown: bool,
    properties: serde_json::Value,
    folder: Option<String>,
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    saved: Option<String>,
}

/// The state of a line's folder in `overlay list` and `overlay repair` (one rule for both).
pub(crate) const NOT_CACHE_PATH: &str = "not a cache path in this line";

fn folder_in_line(
    repo: &Repo,
    l: &crate::model::Line,
    name: &str,
    folder: &str,
    kind: FolderIn,
    with_size: bool,
    op: Option<&Project>,
) -> InLine {
    let cp = repo.working(l).join(folder);
    let is_layer = btrfs::is_subvolume(&cp);
    let state = match (kind, is_layer) {
        (FolderIn::Versioned, true) => format!("not a cache: has versioned files (run layr overlay repair {name})"),
        (FolderIn::Versioned, false) => crate::cmd::admin::VERSIONED.to_string(),
        (FolderIn::Other, _) => NOT_CACHE_PATH.to_string(),
        (FolderIn::Cache, true) => "cache".to_string(),
        (FolderIn::Cache, false) if cp.exists() => format!("plain folder (run layr overlay repair {name})"),
        (FolderIn::Cache, false) => "absent".to_string(),
    };
    let size = listing_size(with_size, kind, is_layer, || crate::overlay::tree_size(&cp));
    // The line on the overlay has the name of the line (docs/overlays.md section 1).
    let saved = op.and_then(|p| crate::overlay::find_follow_line(&p.db, l).ok().flatten()).map(|ol| ids::short(&ol.head));
    InLine { state, size, saved }
}

/// Invoke the cache walk only for an explicitly requested, active cache layer.
fn listing_size(with_size: bool, kind: FolderIn, is_layer: bool, measure: impl FnOnce() -> u64) -> Option<u64> {
    (with_size && kind == FolderIn::Cache && is_layer).then(measure)
}

/// Read settings through the listing's existing overlay handle.
fn listing_settings(settings: &crate::db::Db, name: &str) -> Result<(Props, Option<String>)> {
    let get = |suffix: &str| settings.overlay_config(&format!("overlay.{name}.{suffix}"));
    let mut values = BTreeMap::new();
    for prop in PROPS {
        if let Some(value) = get(prop)? {
            values.insert(*prop, value);
        }
    }
    let props = Props::from_settings(|key| values.get(key).cloned());
    let folder = get("folder")?.and_then(|v| v.as_str().map(str::to_owned));
    Ok((props, folder))
}

fn list(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").flag("--size"), args)?;
    let mut rows = Vec::new();
    let h = here(ctx)?;
    let repo = &h.repo;
    let overlays = repo.listed_overlays()?;
    let shown: BTreeSet<String> = match &h.line {
        Some(l) => repo.listed_layers(l, &overlays)?.into_iter().map(|x| x.overlay).collect(),
        None => BTreeSet::new(),
    };
    for overlay in overlays {
        if !overlay.visible_to(ctx) {
            continue;
        }
        let crate::overlay::ListedOverlay { name, info, project } = overlay;
        let op = project.ok();
        let readers = op.as_ref().map(reader_list).unwrap_or_default();
        let mark = if shown.contains(&name) { "* " } else { "  " };
        // Hidden settings live in the overlay; public settings live in the parent.
        let settings = if info.hidden {
            &op.as_ref().ok_or_else(|| exit(1, format!("error: no local project for hidden overlay '{name}'")))?.db
        } else {
            &repo.p.db
        };
        let (props, folder) = listing_settings(settings, &name)?;
        let line_state = match (&h.line, &folder) {
            (Some(l), Some(f)) => {
                let kind = repo.folder_kind(l, f)?;
                Some(folder_in_line(repo, l, &name, f, kind, a.has("--size"), op.as_ref()))
            }
            _ => None,
        };
        if a.has("--json") {
            rows.push(ListingRow {
                name: name.clone(),
                mode: info.mode.name().to_string(),
                hidden: info.hidden,
                owner: info.owner,
                readers,
                shown: shown.contains(&name),
                properties: props.to_json(),
                folder,
                state: line_state.as_ref().map(|s| s.state.clone()),
                size: line_state.as_ref().and_then(|s| s.size),
                saved: line_state.and_then(|s| s.saved),
            });
        } else {
            let owner = crate::ctx::Caller::from_uid(info.owner).user;
            let mut described = props.describe();
            if let Some(f) = &folder {
                described.insert(0, format!("folder={f}"));
            }
            let keys = settings
                .overlay_config(&format!("overlay.{name}.key-files"))?
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_default();
            if !keys.is_empty() {
                described.push(format!("key-files={}", keys.join(",")));
            }
            // A folder overlay in the line: its state, its size and its saved state (was `cache list -v`).
            let tail = line_state
                .map(|s| {
                    let size = s.size.map(crate::cmd::admin::human_size).unwrap_or_else(|| "-".into());
                    let saved = s.saved.map(|x| format!("saved {x}")).unwrap_or_else(|| "not saved".into());
                    format!("\t{}\t{size}\t{saved}", s.state)
                })
                .unwrap_or_default();
            outln!(
                ctx,
                "{mark}{name}\t{}{}\towner {owner}\treaders {}{}{tail}",
                info.mode.name(),
                if info.hidden { " hidden" } else { "" },
                readers.join(","),
                if described.is_empty() { String::new() } else { format!("\t{}", described.join(" ")) }
            );
        }
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
    }
    Ok(0)
}

/// `layr overlay repair <overlay>`: in the current line, the folder of a folder overlay that a tool
/// made again as a plain folder becomes a cache again. A cache folder with versioned files becomes
/// a plain folder again (`layr cache repair` before; docs/cache-overlays.md, section 4).
fn repair(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let name = match a.pos.as_slice() {
        [name] => name.clone(),
        _ => return Err(exit(128, "usage: layr overlay repair <overlay>")),
    };
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.need_overlay(&name)?;
    let folder = repo
        .overlay_setting(&name, "folder")?
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| exit(128, format!("fatal: overlay '{name}' is no folder overlay")))?;
    let l = h.line()?.clone();
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let l = repo.line(&l.id)?;
    let (u, g) = repo.owner_ids(&l);
    crate::repo::recover_cache_replacements(&repo.working(&l))?;
    match repo.folder_kind(&l, &folder)? {
        FolderIn::Versioned => {
            if crate::repo::uncache(&repo.working(&l), &folder, u, g)? {
                outln!(ctx, "{folder}: {}", crate::cmd::admin::VERSIONED_AGAIN);
            }
        }
        FolderIn::Cache => {
            if crate::repo::convert_cache(&repo.working(&l), &folder, u, g)? {
                outln!(ctx, "{folder}: a cache again");
            }
        }
        FolderIn::Other => outln!(ctx, "{folder}: {NOT_CACHE_PATH}"),
    }
    Ok(0)
}

/// `layr overlay remove <name>`: remove a cache overlay from the project (docs/cache-overlays.md,
/// section 18). The record drops the overlay from the project and from every line's stack; the
/// cache folders of the working folders become plain folders with their files. Its states go at gc.
fn remove(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let name = match a.pos.as_slice() {
        [name] => name.clone(),
        _ => return Err(exit(128, "usage: layr overlay remove <cache overlay>")),
    };
    let h = here(ctx)?;
    let repo = &h.repo;
    let info = visible_info(ctx, repo, &name)?;
    if !repo.regenerable_overlay(&name)? {
        return Err(exit(
            1,
            format!("error: overlay '{name}' has no platform: remove takes only an overlay of build output (property platform), its files are history"),
        ));
    }
    let orepo = repo.overlay_repo(&name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, format!("permission denied: only the owner of overlay '{name}' removes it")));
    }
    crate::overlay_managers::of(repo, &name)?.require(&ctx.store.machine.id)?;
    let folder = repo.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned));
    let _lock = repo.p.lock()?;
    // The overlay's settings go (null deletes a setting); the marker keeps its root for gc. The
    // marker is a string, not an object: an object would read as an overlay of that name.
    let prefix = format!("overlay.{name}");
    let mut cfg = serde_json::Map::new();
    for (key, _) in repo.p.db.config_all()? {
        let base = key.split('@').next().unwrap_or("").to_string();
        if base == prefix || base.starts_with(&format!("{prefix}.")) {
            cfg.insert(base, serde_json::Value::Null);
        }
    }
    cfg.insert(format!("overlay.{name}.removed"), serde_json::json!(info.root));
    let mut rec = Record::new("config");
    rec.data = serde_json::json!({"config": cfg});
    repo.commit_record(ctx, rec)?;
    // The stacks of the lines drop the overlay, and pending moves into it are cancelled.
    for line in repo.lines()? {
        let layers = repo.layers(&line)?;
        if layers.iter().any(|l| l.overlay == name) {
            let kept: Vec<Layer> = layers.into_iter().filter(|l| l.overlay != name).collect();
            let mut rec = Record::new("overlay.use");
            rec.data = serde_json::json!({
                "folder_view": repo.public_view_data(&line.id, &kept)?,
            });
            repo.commit_record(ctx, rec)?;
        }
        if line.machine == ctx.store.machine.id {
            let mut pending = repo.pending_moves(&line);
            if pending.values().any(|o| *o == name) {
                pending.retain(|_, o| *o != name);
                repo.set_pending_moves(&line, &pending)?;
            }
        }
    }
    // The cache folders of the working folders of this machine become plain folders with their files.
    if let Some(folder) = folder {
        for line in repo.lines()? {
            let w = repo.working(&line);
            if line.machine != ctx.store.machine.id || !w.exists() {
                continue;
            }
            if repo.authorize_line(ctx, &line, crate::repo::LineOp::Save).is_err() {
                eprintln!("warning: line {} keeps {folder} as a cache: its owner saves it as a plain folder", line.name);
                continue;
            }
            let (u, g) = repo.owner_ids(&line);
            crate::repo::uncache(&w, &folder, u, g)?;
        }
    }
    outln!(ctx, "Removed cache overlay {name}");
    Ok(0)
}

fn readers(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let info = visible_info(ctx, &h.repo, &name)?;
    let orepo = h.repo.overlay_repo(&name)?;
    if a.pos.len() == 1 {
        for r in reader_list(&orepo.p) {
            outln!(ctx, "{r}");
        }
        return Ok(0);
    }
    if !orepo.is_admin(ctx) {
        return Err(exit(1, format!("permission denied: only the owner of overlay '{name}' sets its readers")));
    }
    let list = principals(&a.pos[1..])?;
    let _parent_lock = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    crate::overlay_managers::of(&h.repo, &name)?.require(&ctx.store.machine.id)?;
    let access = access_map(info.owner, &list);
    let removed = orepo.policy().access.iter().any(|(principal, role)| *role >= Role::Reader && access.get(principal).is_none());
    let mut record = Record::new("overlay.readers");
    record.data = serde_json::json!({"config":{"access":access}});
    if removed {
        record.data["config"]["overlay.revocation"] = serde_json::json!(ids::new_id());
    }
    orepo.commit_record(ctx, record)?;
    Ok(0)
}

fn access_mode(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if a.pos.len() != 2 {
        return Err(exit(128, "usage: layr overlay mode <name> private|service|public"));
    }
    let h = here(ctx)?;
    let name = &a.pos[0];
    let info = visible_info(ctx, &h.repo, name)?;
    let mode = layr_store::epochs::Mode::parse(&a.pos[1])?;
    let parent = crate::store_remote::epochs::project_mode(&h.repo)?;
    if mode > parent {
        return Err(exit(1, format!("overlay mode {} is more open than project mode {}", mode.name(), parent.name())));
    }
    if info.hidden && mode != layr_store::epochs::Mode::Private {
        return Err(exit(1, "hidden overlays are always private"));
    }
    let orepo = h.repo.overlay_repo(name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can set its access mode"));
    }
    crate::overlay_managers::of(&h.repo, name)?.require(&ctx.store.machine.id)?;
    if mode == layr_store::epochs::Mode::Service {
        let service = h.repo.p.config_value("service.recipient")?.and_then(|v| v.as_str().map(str::to_owned));
        if service.as_ref().is_none_or(|s| !layr_store::keys::valid_recipient(s)) {
            return Err(exit(1, "service overlay requires layr config service.recipient <age recipient>"));
        }
    }
    let _parent = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let old = crate::store_remote::epochs::overlay_mode(&orepo)?;
    if old != mode {
        let mut record = Record::new("overlay.mode");
        record.data = serde_json::json!({"mode":mode,"config":{"overlay.access-mode":mode}});
        if mode < old {
            record.data["config"]["overlay.revocation"] = serde_json::json!(ids::new_id());
        }
        orepo.commit_record(ctx, record)?;
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::json!({"name":name,"access_mode":mode}));
    } else {
        outln!(ctx, "overlay {name}: {} (takes effect on the next push)", mode.name());
    }
    Ok(0)
}

fn managers(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?;
    visible_info(ctx, &h.repo, name)?;
    let orepo = h.repo.overlay_repo(name)?;
    let _parent_lock = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let authority = crate::overlay_managers::of(&h.repo, name)?;
    if a.pos.len() == 1 {
        for machine in authority.machines {
            outln!(ctx, "machine:{machine}");
        }
        return Ok(0);
    }
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can change key managers"));
    }
    authority.require(&ctx.store.machine.id)?;
    let mut machines = BTreeSet::new();
    for principal in &a.pos[1..] {
        let machine = principal
            .strip_prefix("machine:")
            .filter(|id| crate::model::is_uuid(id))
            .ok_or_else(|| exit(128, "use machine:<UUID> for each key manager"))?;
        if !crate::store_remote::overlays::trusted_keys(&h.repo)?.contains_key(machine) {
            return Err(exit(1, format!("key manager machine {machine} is not trusted")));
        }
        machines.insert(machine.to_string());
    }
    if !machines.contains(&ctx.store.machine.id) {
        return Err(exit(1, "a remaining key manager must sign manager removal"));
    }
    let change = crate::overlay_managers::Change {
        parent: h.repo.p.id.clone(),
        overlay: orepo.p.id.clone(),
        name: name.clone(),
        machines,
        previous: authority.previous.clone(),
        retained: crate::overlay_managers::retained(&authority, &orepo)?,
    };
    let data = serde_json::to_value(change)?;
    let mut pending = None;
    for line in orepo.p.export_jsonl()?.lines() {
        let record = crate::oplog::parse_line(line, None)?.record;
        if record.op == "overlay.managers"
            && record.machine == ctx.store.machine.id
            && record.data["previous"] == data["previous"]
        {
            if record.data["machines"] != data["machines"] {
                return Err(exit(1, "retry the pending manager change before another change"));
            }
            pending = Some(line.to_string());
        }
    }
    let signed = if let Some(signed) = pending {
        signed
    } else {
        let mut record = Record::new("overlay.managers");
        record.data = data;
        let record = orepo.p.append(record, &ctx.actor())?;
        orepo.p.db.record_line(&record.machine, record.seq)?.ok_or_else(|| exit(1, "manager record is missing"))?
    };
    if h.repo.need_overlay(name)?.hidden {
        return Ok(0);
    }
    let mut parent = Record::new("overlay.authority");
    parent.data = serde_json::json!({"name":name,"authority":signed});
    h.repo.commit_record(ctx, parent)?;
    Ok(0)
}

fn visible_info(ctx: &Ctx, repo: &Repo, name: &str) -> Result<crate::overlay::Info> {
    let info = repo.need_overlay(name)?;
    if info.hidden && !repo.reads_overlay(name, ctx.caller.uid, ctx.caller.gid)? {
        return Err(exit(1, format!("error: no overlay '{name}' in project {}", repo.p.name)));
    }
    Ok(info)
}

/// Public metadata only: never serialize a complete key or operation payload.
fn reader_detail(principal: &str) -> serde_json::Value {
    let kind = match principal.split_once(':').map(|(kind, _)| kind) {
        Some("user") => "user",
        Some("group") => "group",
        Some("machine") => "machine",
        Some("person") => "person",
        _ => "everyone",
    };
    // Person policy entries include an age recipient; the login alone identifies the reader.
    let principal = if kind == "person" { principal.split('=').next().unwrap_or(principal) } else { principal };
    serde_json::json!({"principal":principal,"kind":kind})
}

fn operation_detail(r: &Record) -> serde_json::Value {
    serde_json::json!({"id":r.id,"op":super::admin::operation_name(&r.op),"actor":r.actor,"time":r.time})
}

/// Signed records in their application order, independent of UUID spelling and machine export order.
fn own_records(repo: &Repo) -> Result<Vec<Record>> {
    repo.p.db.all_lines()?.iter().map(|line| Ok(crate::oplog::parse_line(line, None)?.record)).collect()
}

fn show_json(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json"), args)?;
    if a.pos.len() != 1 || !a.paths.is_empty() {
        return Err(exit(128, USAGE));
    }
    let name = &a.pos[0];
    let h = here(ctx)?;
    let info = visible_info(ctx, &h.repo, name)?;
    let orepo = Repo::open(ctx, &format!("{}%{name}", h.repo.p.name))?;
    let records = own_records(&orepo)?;
    let readers: Vec<_> = orepo
        .policy()
        .access
        .iter()
        .filter(|(_, role)| **role >= Role::Reader)
        .map(|(principal, _)| reader_detail(principal))
        .collect();
    let setting = |suffix: &str| -> Result<Option<serde_json::Value>> {
        let key = format!("overlay.{name}.{suffix}");
        if info.hidden {
            orepo.p.db.overlay_config(&key)
        } else {
            h.repo.p.config_value(&key)
        }
    };
    // The state that the current line shows on the overlay (its view), which is the newest also when an overlay-only
    // save on a protected line keeps it in the stack; else the head of the overlay's line.
    let shown = match h.line.as_ref() {
        Some(line) => h.repo.layers(line)?.into_iter().find(|l| &l.overlay == name).map(|l| l.state),
        None => None,
    };
    let state = match shown {
        Some(s) => s,
        None => overlay_line(ctx, &h.repo, h.line.as_ref(), &info, name, &orepo)?.map(|l| l.head).unwrap_or(info.root.clone()),
    };
    let value = serde_json::json!({
        "name":name,"hidden":info.hidden,"readers":readers,"access_mode":crate::store_remote::epochs::overlay_mode(&orepo)?,
        "managers":crate::overlay_managers::of(&h.repo, name)?.machines,
        "epochs":layr_model::overlays::epochs(&records),
        "patterns":setting("patterns")?.unwrap_or_else(|| serde_json::json!([])),
        "peers":setting("peers")?,"state":state,"properties":h.repo.overlay_props(name)?.to_json(),
    });
    outln!(ctx, "{}", serde_json::to_string(&value)?);
    Ok(0)
}

fn operations(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--ops").flag("--json"), args)?;
    if a.pos.len() != 1 || !a.paths.is_empty() {
        return Err(exit(128, USAGE));
    }
    let name = &a.pos[0];
    let h = here(ctx)?;
    visible_info(ctx, &h.repo, name)?;
    let orepo = Repo::open(ctx, &format!("{}%{name}", h.repo.p.name))?;
    let records = own_records(&orepo)?;
    if a.has("--json") {
        let rows: Vec<_> = records.iter().rev().map(operation_detail).collect();
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
    } else {
        for r in records.iter().rev() {
            outln!(
                ctx,
                "{} {} {} {}",
                r.id,
                super::history::fmt_date(r.time, "iso"),
                r.actor.name,
                super::admin::operation_name(&r.op)
            );
        }
    }
    Ok(0)
}

fn show_to_project(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let name = a.pos.first().ok_or_else(|| exit(128, USAGE))?;
    let info = visible_info(ctx, &h.repo, name)?;
    let orepo = h.repo.overlay_repo(name)?;
    if !orepo.is_admin(ctx) {
        return Err(exit(1, "only the overlay owner can show it to the project"));
    }
    let _parent = h.repo.p.lock()?;
    let _lock = orepo.p.lock()?;
    let own_lines: Vec<_> = orepo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let creator = own_lines.iter().find(|line| {
        crate::oplog::parse_line(line, None).is_ok_and(|s| {
            s.record.op == "overlay.create" && s.record.data["config"][format!("overlay.{name}")]["hidden"] == true
        })
    });
    let Some(creator) = creator else {
        return Ok(0);
    };
    let authority = layr_model::authority::from_overlay_lines(
        &h.repo.p.id,
        &orepo.p.id,
        &own_lines,
        &crate::store_remote::overlays::trusted_keys(&h.repo)?,
    )?
    .1;
    authority.require(&ctx.store.machine.id)?;
    let mut metadata = orepo.p.db.overlay_config("overlay")?.ok_or_else(|| exit(1, "overlay metadata is missing"))?;
    let mut public =
        orepo.p.db.overlay_config(&format!("overlay.{name}"))?.ok_or_else(|| exit(1, "overlay creation is missing"))?;
    public["hidden"] = serde_json::json!(false);
    if metadata["hidden"] != false {
        metadata["hidden"] = serde_json::json!(false);
        let mut show = Record::new("overlay.show-to-project");
        show.data = serde_json::json!({"name":name,"config":{"overlay":metadata,format!("overlay.{name}"):public}});
        orepo.commit_record(ctx, show)?;
    }
    let parent_lines: Vec<_> = h.repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let parent_records: Vec<_> =
        parent_lines.iter().map(|line| crate::oplog::parse_line(line, None).map(|s| s.record)).collect::<Result<_>>()?;
    if !parent_records.iter().any(|r| r.op == "overlay.create" && r.data["name"] == *name) {
        let mut create = Record::new("overlay.create");
        create.data = serde_json::json!({"name":name,"config":{format!("overlay.{name}"):public},"hidden_origin":creator});
        h.repo.commit_record(ctx, create)?;
    }
    for line in &own_lines {
        if crate::oplog::parse_line(line, None)?.record.op == "overlay.managers"
            && !parent_records.iter().any(|r| r.op == "overlay.authority" && r.data["authority"] == *line)
        {
            let mut proof = Record::new("overlay.authority");
            proof.data = serde_json::json!({"name":name,"authority":line});
            h.repo.commit_record(ctx, proof)?;
        }
    }
    for (key, value) in orepo.p.db.config_all()? {
        if key.starts_with(&format!("overlay.{name}.")) {
            let value: serde_json::Value = serde_json::from_str(&value)?;
            if h.repo.p.db.overlay_config(&key)?.as_ref() != Some(&value) {
                super::admin::set_config(ctx, &h.repo, &key, value)?;
            }
        }
    }
    // Public views start naming the overlay only after the show operation: a line that showed the
    // hidden overlay shows it at the same place.
    for line in h.repo.lines()? {
        let selection = crate::overlay::hidden_view(&orepo.p, &line.id)?;
        if crate::overlay::find_overlay_line(&orepo.p.db, &info, &line)?.is_none() || selection.as_ref().is_some_and(|v| v["enabled"] == false) {
            continue;
        }
        let mut names: Vec<String> = h.repo.layers(&line)?.into_iter().map(|l| l.overlay).collect();
        if !names.contains(name) {
            let position = selection.as_ref().and_then(|v| v["position"].as_u64()).unwrap_or(u64::MAX);
            names.insert((position as usize).min(names.len()), name.clone());
            let mut record = Record::new("overlay.use");
            record.data = serde_json::json!({"folder_view": crate::overlay::view_data(&line.id, &names)});
            h.repo.commit_record(ctx, record)?;
        }
    }
    outln!(ctx, "Overlay {name} is visible to the project");
    Ok(0)
}

/// `layr overlay migrate-parts` (docs/overlays.md, section 1a): the head of each line names its
/// parts on the overlays that keep parts, from the line's stack (the heads of its overlay lines). One
/// `state.parts` record adds them; the signed save records stay as they are. A state that two lines
/// show takes the parts of the line that saved it, else of the first line by name. Older states
/// keep no parts: their overlay lines stay for reading.
fn migrate_parts(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-n|--dry-run").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    if !a.pos.is_empty() {
        return Err(exit(128, USAGE));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    if !repo.is_admin(ctx) {
        return Err(exit(1, "error: only an admin of the project can migrate its states to parts"));
    }
    // A `cache.<folder>` overlay of an older layr gets the platform of its states, so it is the overlay of that platform.
    // `overlay set` takes the project lock itself, so this comes before the lock below.
    let platforms = old_cache_platforms(repo)?;
    if !a.has("-n") {
        for (overlay, platform) in &platforms {
            set(ctx, &[overlay.clone(), "--platform".into(), platform.clone()])?;
        }
    }
    if !a.has("-q") {
        let verb = if a.has("-n") { "would set" } else { "set" };
        for (overlay, platform) in &platforms {
            outln!(ctx, "{verb} platform {platform} on {overlay}");
        }
    }
    let _lock = repo.p.lock()?;
    // Each line takes its name on every overlay, its old stack (layr before 0.44) becomes the
    // view of its folder and the heads of its lines on the overlays, and no overlay keeps the
    // `cache` flag: a cache is an ordinary overlay (docs/overlays.md section 1a).
    let moved_lines = move_old_lines(ctx, repo, a.has("-n"))?;
    if !a.has("-q") {
        let verb = if a.has("-n") { "would move" } else { "moved" };
        for m in &moved_lines {
            outln!(ctx, "{verb} {m}");
        }
    }
    let mut lines = repo.lines()?;
    let mut saved_by = BTreeMap::new();
    for l in &lines {
        if let Some(st) = repo.p.db.state(&l.head)? {
            saved_by.insert(l.head.clone(), st);
        }
    }
    // The line that saved a state goes first, then the lines by name.
    lines.sort_by_key(|l| (saved_by.get(&l.head).and_then(|s| s.line.as_deref()) != Some(l.id.as_str()), l.name.clone()));
    let mut states: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut moved = BTreeSet::new();
    for l in &lines {
        let Some(st) = saved_by.get(&l.head) else { continue };
        let layers = match repo.layers(l) {
            Ok(layers) => layers,
            Err(e) => {
                eprintln!("warning: line '{}' keeps its overlay lines: {e:#}", l.name);
                continue;
            }
        };
        let mut parts = repo.parts_of(&layers)?;
        // The folder overlays (build output): the head of the line's overlay line of this platform.
        for (overlay, part) in folder_parts(repo, l)? {
            parts.entry(overlay).or_insert(part);
        }
        let entry = states.entry(l.head.clone()).or_default();
        for (overlay, part) in parts {
            if !st.parts.contains_key(&overlay) && !entry.contains_key(&overlay) {
                entry.insert(overlay, part);
                moved.insert(l.name.clone());
            }
        }
    }
    states.retain(|_, parts| !parts.is_empty());
    if states.is_empty() {
        if !a.has("-q") {
            outln!(ctx, "no state to migrate: every line head names its parts");
        }
        return Ok(0);
    }
    if !a.has("-n") {
        let mut rec = Record::new(layr_model::validation::STATE_PARTS_OP);
        rec.data = serde_json::json!({ "states": states });
        repo.commit_record(ctx, rec)?;
    }
    if !a.has("-q") {
        for (state, parts) in &states {
            let list: Vec<String> = parts.iter().map(|(g, p)| format!("{g}={}", ids::short(p))).collect();
            outln!(ctx, "{} {}", ids::short(state), list.join(" "));
        }
        let verb = if a.has("-n") { "would give" } else { "gave" };
        outln!(ctx, "{verb} parts to {} state(s) of {} line(s)", states.len(), moved.len());
    }
    Ok(0)
}

/// The lines and overlays of layr before 0.44, moved to the model of docs/overlays.md section 1: a
/// line of an overlay takes the name of its line (`line-<id>` and `line-<id>.<platform>` go), the
/// old stack of a line (`stack.<line id>`) becomes the view of its folder and the heads of its
/// lines on the overlays, and the `cache` flag of an overlay goes (its `trust` stays `protected`).
/// Returns what it moved, as lines of text. With `dry`, it changes nothing. The caller holds the
/// project lock.
fn move_old_lines(ctx: &Ctx, repo: &Repo, dry: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let lines = repo.lines()?;
    for l in &lines {
        let old = repo.old_stack(l)?;
        if !old.is_empty() && repo.folder_view(l)?.is_none() {
            out.push(format!("the stack of line {} to its view", l.name));
            if !dry {
                let mut rec = Record::new("overlay.use");
                rec.data = serde_json::json!({"folder_view": repo.public_view_data(&l.id, &old)?});
                repo.commit_record(ctx, rec)?;
            }
        }
        for o in repo.overlay_names()? {
            let Some(info) = repo.overlay_info(&o)? else { continue };
            // An overlay of every platform keeps its lines per platform (`ensure_build_line`).
            if info.mode != Mode::Follow || repo.per_platform_lines(&o)? {
                continue;
            }
            let Ok(orepo) = repo.overlay_repo(&o) else { continue };
            let found = crate::overlay::find_overlay_line(&orepo.p.db, &info, l)?;
            let stacked = old.iter().find(|x| x.overlay == o);
            if let Some(ol) = found.as_ref().filter(|ol| ol.name != l.name) {
                out.push(format!("line {} of overlay {o} to {}", ol.name, l.name));
            }
            if let Some(s) = stacked.filter(|s| found.as_ref().is_none_or(|ol| ol.head != s.state)) {
                out.push(format!("line {} of overlay {o} to {}", l.name, ids::short(&s.state)));
            }
            if dry || (found.is_none() && stacked.is_none()) {
                continue;
            }
            let _ol_lock = orepo.p.lock()?;
            let start = stacked.map(|s| s.state.clone()).or(found.as_ref().map(|ol| ol.head.clone())).unwrap_or(info.root.clone());
            let ol = orepo.ensure_overlay_line(ctx, &info, l, &start)?;
            if ol.head != start {
                crate::overlay::move_overlay_line(&orepo, ctx, "overlay.migrate", &ol, &start, Vec::new())?;
            }
        }
    }
    let mut cfg = serde_json::Map::new();
    for o in repo.overlay_names()? {
        if repo.p.db.overlay_config(&format!("overlay.{o}.cache"))?.and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        out.push(format!("overlay {o}: an ordinary overlay (no cache flag)"));
        cfg.insert(format!("overlay.{o}.cache"), serde_json::Value::Null);
        if repo.overlay_props(&o)?.trust.is_none() {
            cfg.insert(format!("overlay.{o}.trust"), serde_json::json!(crate::overlay::Trust::Protected.name()));
        }
    }
    if !dry && !cfg.is_empty() {
        let mut rec = Record::new("config");
        rec.data = serde_json::json!({ "config": cfg });
        repo.commit_record(ctx, rec)?;
    }
    Ok(out)
}

/// The parts of line `l` on the folder overlays of this platform: the head of its overlay line (`line-<id>`, or the
/// older `line-<id>.<platform>`), when that is not the empty first state.
fn folder_parts(repo: &Repo, l: &crate::model::Line) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (overlay, _) in repo.folder_overlays()? {
        let (Some(info), Ok(orepo)) = (repo.overlay_info(&overlay)?, repo.overlay_repo(&overlay)) else { continue };
        if let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, l)? {
            if ol.head != info.root {
                out.insert(overlay.clone(), ol.head);
            }
        }
    }
    Ok(out)
}

/// The `cache.<folder>` overlays with no `platform` whose saved states all have one platform: overlay and platform.
/// An overlay with states of two platforms keeps no platform (`fetch` and `line new` filter its states one by one).
fn old_cache_platforms(repo: &Repo) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (overlay, _, platform) in repo.all_folder_overlays()? {
        if platform.is_some() || !overlay.starts_with("cache.") {
            continue;
        }
        let Ok(orepo) = repo.overlay_repo(&overlay) else { continue };
        let mut seen = BTreeSet::new();
        for (id, _) in orepo.p.db.cache_states()? {
            if let Some(st) = orepo.p.db.state(&id)? {
                seen.extend(st.cache_platform);
            }
        }
        if seen.len() == 1 {
            out.push((overlay, seen.into_iter().next().unwrap_or_default()));
        }
    }
    Ok(out)
}

/// The layer of overlay `name` for a line of `owner`: its overlay line, made when missing (a
/// follow line starts empty; a shared line of another user starts at the owner's).
fn layer_for(ctx: &Ctx, repo: &Repo, name: &str, line: &crate::model::Line) -> Result<Layer> {
    let info = repo.need_overlay(name)?;
    let gid = crate::ctx::Caller::from_uid(line.owner).gid;
    if !repo.reads_overlay(name, line.owner, gid)? {
        return Err(exit(1, format!("error: the owner of line '{}' does not read overlay '{name}'", line.name)));
    }
    // The line on the overlay: its head, else the part that the head of the line names, else the
    // empty first state. The line on the overlay is made at its first write.
    if info.mode == Mode::Follow {
        let _ = ctx;
        return repo.overlay_layer(Some(line), &line.head, name)?.ok_or_else(|| exit(1, format!("error: no overlay '{name}'")));
    }
    let orepo = repo.overlay_repo(name)?;
    let _lock = orepo.p.lock()?;
    let start = orepo.p.db.line_by_name(&format!("user-{}", info.owner))?.map(|l| l.head).unwrap_or(info.root.clone());
    let ol = orepo.ensure_overlay_line(ctx, &info, line, &start)?;
    Ok(Layer { overlay: name.to_string(), state: ol.head })
}

/// `layr overlay order [<name>,...]`: show or set the project order of overlays (setting
/// `overlays.order`). A new line folder shows its overlays in this order (docs/overlays.md, section
/// 1a, slice 5); overlays that the list does not name follow in the order of their creation.
fn order(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if repo.p.is_overlay() {
        return Err(exit(1, "error: an overlay has no overlays"));
    }
    let names: Vec<String> =
        a.pos.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    if names.is_empty() {
        for name in repo.project_order()? {
            let hidden = repo.need_overlay(&name).map(|i| i.hidden).unwrap_or(true);
            if !hidden || repo.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                outln!(ctx, "{name}");
            }
        }
        return Ok(0);
    }
    repo.require(ctx, "config")?;
    let _lock = repo.p.lock()?;
    for (i, name) in names.iter().enumerate() {
        let info = repo.overlay_info(name)?.ok_or_else(|| exit(1, format!("error: no overlay '{name}'")))?;
        // The order is a public setting: a hidden name would show to every reader of the project.
        if info.hidden {
            return Err(exit(1, format!("error: overlay '{name}' is hidden; a hidden overlay keeps the order of its creation")));
        }
        if names[..i].contains(name) {
            return Err(exit(128, format!("error: overlay '{name}' is named twice")));
        }
    }
    let mut rec = Record::new("config");
    rec.data = serde_json::json!({"config": {crate::overlay::ORDER_KEY: names}});
    repo.commit_record(ctx, rec)?;
    Ok(0)
}

/// Set the view of the working folder of the current line: the overlays it shows and their order
/// (`view.<line id>`, slice 5). The owner's choice: the line's overlay files come and go.
fn use_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--add").flag("--remove").flag("--none").flag("-f|--force").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    if a.pos.is_empty() && !a.has("--none") {
        let layers: Vec<_> = repo
            .layers(&line)?
            .into_iter()
            .filter(|l| {
                !repo.need_overlay(&l.overlay).map(|i| i.hidden).unwrap_or(true)
                    || repo.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
            })
            .collect();
        if layers.is_empty() {
            outln!(ctx, "line {} shows no overlays", line.name);
        }
        for l in layers {
            outln!(ctx, "{}\t{}", l.overlay, ids::short(&l.state));
        }
        return Ok(0);
    }
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let line = repo.line(&line.id)?;
    let cur = repo.layers(&line)?;
    let mut names: Vec<String> = Vec::new();
    if a.has("--add") || a.has("--remove") {
        names.extend(cur.iter().map(|l| l.overlay.clone()));
    }
    for n in &a.pos {
        refuse_folder(&h.repo, n)?;
        if a.has("--remove") {
            names.retain(|x| x != n);
        } else if !names.contains(n) {
            names.push(n.clone());
        }
    }
    if a.has("--none") {
        names.clear();
    }
    // Named overlays take their newest state; with --add and --remove the others stay as shown.
    let keep = a.has("--add") || a.has("--remove");
    let mut layers = Vec::new();
    for n in &names {
        match cur.iter().find(|l| &l.overlay == n) {
            Some(l) if keep && !a.pos.contains(n) => layers.push(l.clone()),
            _ => layers.push(layer_for(ctx, repo, n, &line)?),
        }
    }
    change_stack(ctx, repo, &line, &layers, a.has("-f"))?;
    if !a.has("-q") {
        if layers.is_empty() {
            outln!(ctx, "line {} shows no overlays", line.name);
        } else {
            outln!(ctx, "line {} shows overlays: {}", line.name, names.join(" "));
        }
    }
    Ok(0)
}

fn change_stack(ctx: &Ctx, repo: &Repo, line: &crate::model::Line, layers: &[Layer], force: bool) -> Result<()> {
    if repo.layers(line)? == layers {
        return Ok(());
    }
    let (st, _scan, _it, _idx) = repo.status(line)?;
    if !st.staged.is_empty() {
        return Err(exit(1, "error: the line has staged changes; save them or unstage them ('layr restore --staged') first"));
    }
    for (p, o) in repo.pending_moves(line) {
        if !layers.iter().any(|l| l.overlay == o) {
            return Err(exit(
                1,
                format!(
                    "error: '{p}' is to move to overlay '{o}' at the next state; save first, or 'layr overlay move --cancel {p}'"
                ),
            ));
        }
    }
    crate::overlay::set_layers(ctx, repo, line, layers, force, "overlay.use")?;
    Ok(())
}

/// Move files of the current line into an overlay: the next commit takes them out of the layer
/// that has them and into the overlay's history.
fn move_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--purge").flag("--cancel").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line()?.clone();
    if a.has("--cancel") {
        repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
        let _lock = repo.p.lock()?;
        let specs = h.specs(&a.rest())?;
        let mut pending = repo.pending_moves(&line);
        pending.retain(|p, _| !(specs.is_empty() || super::matches(&specs, p)));
        repo.set_pending_moves(&line, &pending)?;
        return Ok(0);
    }
    if a.rest().len() < 2 {
        return Err(exit(128, USAGE));
    }
    let name = a.rest()[0].clone();
    let specs = h.specs(&a.rest()[1..])?;
    repo.need_overlay(&name)?;
    refuse_folder(repo, &name)?;
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Save)?;
    let _lock = repo.p.lock()?;
    let mut line = repo.line(&line.id)?;
    // The line shows the overlay first (its files come into the folder).
    let cur = repo.layers(&line)?;
    if !cur.iter().any(|l| l.overlay == name) {
        let mut layers = cur.clone();
        layers.push(layer_for(ctx, repo, &name, &line)?);
        change_stack(ctx, repo, &line, &layers, false)?;
        line = repo.line(&line.id)?;
    }
    let scan = repo.scan(&line)?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for s in &specs {
        if s.is_empty() || crate::repo::skip_path(s) {
            return Err(exit(128, format!("fatal: cannot move '{}' into an overlay", h.rel(s))));
        }
        match crate::fsutil::lmeta(&scan.path, s) {
            None => return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s)))),
            Some(m) if m.is_dir() => {
                crate::fsutil::walk(&scan.path, s, &mut |rel, m| {
                    if !m.is_dir() {
                        paths.insert(rel.to_string());
                    }
                    Ok(true)
                })?;
            }
            Some(_) => {
                paths.insert(s.clone());
            }
        }
    }
    let mut pending = repo.pending_moves(&line);
    for p in &paths {
        pending.insert(p.clone(), name.clone());
    }
    repo.set_pending_moves(&line, &pending)?;
    // Automatic states of the line made before the move hold the files (they hold the whole
    // working folder, ignored files included), and every reader of the project reads them.
    let mut holding = Vec::new();
    for st in repo.p.db.line_states(&line.id, StateKind::Auto)? {
        let Ok(root) = repo.state_src(&st.id) else {
            continue;
        };
        if paths.iter().any(|p| crate::fsutil::lmeta(&root, p).is_some()) {
            holding.push(st.id);
        }
    }
    if a.has("--purge") && !holding.is_empty() {
        let mut deleted = Vec::new();
        for id in &holding {
            #[cfg(target_os = "macos")]
            let gone = crate::thin::delete_now(&repo.p, id).is_ok();
            #[cfg(not(target_os = "macos"))]
            let gone = repo.p.delete_state(id).is_ok();
            if gone {
                deleted.push(id.clone());
            }
        }
        let mut rec = Record::new("gc");
        rec.data = serde_json::json!({"deleted": deleted, "purge": "overlay move"});
        repo.commit_record(ctx, rec)?;
        if !a.has("-q") {
            outln!(ctx, "deleted {} automatic state(s) of line {} that held the files", deleted.len(), line.name);
        }
    } else if !holding.is_empty() {
        outln!(
            ctx,
            "note: {} automatic state(s) of line {} hold these files, and readers of the project can read them; 'layr overlay move --purge' deletes those states",
            holding.len(),
            line.name
        );
    }
    if !a.has("-q") {
        let shared = repo.state_root(&line.head)?;
        let pushed = !repo.p.db.remote_refs()?.is_empty();
        for p in &paths {
            outln!(ctx, "move to overlay {name} at the next state: {}", h.rel(p));
            if crate::fsutil::lmeta(&shared, p).is_some() {
                outln!(
                    ctx,
                    "note: '{}' is in the shared set: earlier states keep it{}; layr does not rewrite history",
                    h.rel(p),
                    if pushed { ", and so may the git remote" } else { "" }
                );
            }
        }
    }
    Ok(0)
}

fn overlay_line(
    ctx: &Ctx,
    repo: &Repo,
    line: Option<&crate::model::Line>,
    info: &crate::overlay::Info,
    name: &str,
    orepo: &Repo,
) -> Result<Option<crate::model::Line>> {
    // The overlay line of the current line; else the caller's own line, else the owner's.
    if let Some(l) = line {
        if repo.layers(l)?.iter().any(|x| x.overlay == name) {
            if let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, info, l)? {
                return Ok(Some(ol));
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    if info.hidden {
        for line in orepo.lines()? {
            names.push(line.name);
        }
    }
    names.push(format!("user-{}", ctx.caller.uid));
    names.push(format!("user-{}", info.owner));
    Ok(names.into_iter().find_map(|n| orepo.p.db.line_by_name(&n).ok().flatten()))
}

/// A reading command on an overlay's history, as a reader of the overlay: `layr <cmd>` in the
/// overlay's own project, at the overlay line of the current line (or of the caller).
fn read(ctx: &Ctx, cmd: &str, args: &[String]) -> Result<i32> {
    let name = args.first().ok_or_else(|| exit(128, USAGE))?.clone();
    let h = here(ctx)?;
    let info = visible_info(ctx, &h.repo, &name)?;
    let pname = format!("{}%{name}", h.repo.p.name);
    // The overlay's own policy decides: a user who does not read it is refused here.
    let orepo = Repo::open(ctx, &pname)?;
    let olname = overlay_line(ctx, &h.repo, h.line.as_ref(), &info, &name, &orepo)?.map(|l| l.name);
    // An overlay that keeps parts has no overlay line: the commands read from the part that the
    // current line shows, as a head without a line.
    let detached = match (&olname, h.line.as_ref()) {
        (None, Some(l)) if crate::overlay::parts_overlay(&info) => {
            h.repo.layers(l)?.into_iter().find(|x| x.overlay == name).map(|x| crate::model::Line {
                id: ids::new_id(),
                name: l.name.clone(),
                head: x.state,
                owner: l.owner,
                machine: l.machine.clone(),
            })
        }
        _ => None,
    };
    let mut env = ctx.env.clone();
    env.insert("LAYR_PROJECT".into(), pname);
    match olname {
        Some(n) => env.insert("LAYR_LINE".into(), n),
        None => env.remove("LAYR_LINE"),
    };
    use std::io::Write;
    let _ = ctx.out.borrow_mut().flush();
    let sub = Ctx {
        store: crate::store::Store::open(&ctx.store.root)?,
        caller: ctx.caller.clone(),
        cwd: ctx.cwd.clone(),
        env,
        author: Default::default(),
        out: std::cell::RefCell::new(Box::new(std::io::BufWriter::new(std::io::stdout()))),
    };
    let mut argv = vec![cmd.to_string()];
    argv.extend(args[1..].iter().cloned());
    let r = match detached {
        Some(l) => {
            let project = sub.env("LAYR_PROJECT").unwrap_or_default().to_string();
            super::with_detached(&project, l, || super::run(&sub, &argv))
        }
        None => super::run(&sub, &argv),
    };
    let _ = sub.out.borrow_mut().flush();
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn listing_opens_each_overlay_once_and_preserves_visibility() -> Result<()> {
        // Plain project folders keep this command regression independent of subvolume privileges.
        let root = std::env::temp_dir().join(format!("layr-list-opens-{}", ids::new_id()));
        std::fs::create_dir_all(&root)?;
        let result = (|| -> Result<()> {
            let store = crate::store::Store::open(&root.join("store"))?;
            // Visibility exercises the ordinary reader policy even when the test runner is root.
            let caller = crate::ctx::Caller { uid: 1000, gid: 1000, user: "reader".into(), home: root.join("home") };
            let actor = crate::model::Actor { uid: caller.uid, name: caller.user.clone(), session: None };
            let create = |name: &str, config: serde_json::Value| -> Result<Project<'_>> {
                let dir = store.project_dir(name)?;
                std::fs::create_dir_all(dir.join("meta/db"))?;
                std::fs::create_dir_all(dir.join("states"))?;
                std::fs::create_dir_all(dir.join("lines"))?;
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
            let access = json!({format!("user:{}", caller.uid): "admin"});
            let info = |hidden| json!({"hidden":hidden,"owner":caller.uid,"mode":"follow","root":"empty"});
            let parent = create("p", json!({"access":access,"overlay.public":info(false),"overlay.remote":info(false)}))?;
            let line_id = ids::new_id();
            let add_line = |p: &Project, name: &str| -> Result<()> {
                p.db.conn.execute(
                    "INSERT INTO lines (id,name,owner,machine,head,time,ord) VALUES (?1,?2,?3,?4,'empty',0,'1')",
                    (line_id.as_str(), name, caller.uid, store.machine.id.as_str()),
                )?;
                Ok(())
            };
            add_line(&parent, "main")?;
            let cwd = parent.lines_dir().join("main");
            std::fs::create_dir_all(&cwd)?;
            std::fs::write(cwd.join("file"), "keep the working folder nonempty")?;
            create("p%public", json!({"access":access}))?;
            let hidden = create(
                "p%private",
                json!({
                    "access":access,"overlay":{"project":parent.id,"name":"private"},
                    "overlay.private":info(true),"overlay.private.quiet":true,"overlay.private.history":3
                }),
            )?;
            add_line(&hidden, &format!("line-{line_id}"))?;
            create(
                "p%denied",
                json!({
                    "access":{},"overlay":{"project":parent.id,"name":"denied"},"overlay.denied":info(true)
                }),
            )?;
            create(
                "p%foreign",
                json!({
                    "access":access,"overlay":{"project":"another-project","name":"foreign"},"overlay.foreign":info(true)
                }),
            )?;
            drop((hidden, parent));
            let output = root.join("output");
            let ctx = Ctx {
                store,
                caller,
                cwd,
                env: std::collections::HashMap::new(),
                author: Default::default(),
                out: std::cell::RefCell::new(Box::new(std::fs::File::create(&output)?)),
            };
            // Run twice: handles belong to a listing, never to a later command.
            for _ in 0..2 {
                let before = crate::store::project_open_count();
                assert_eq!(super::super::run(&ctx, &["overlay".into(), "list".into(), "--json".into()])?, 0);
                assert_eq!(crate::store::project_open_count() - before, 6, "parent and five overlay candidates");
            }
            let text = std::fs::read_to_string(output)?;
            for line in text.lines() {
                let rows: Vec<serde_json::Value> = serde_json::from_str(line)?;
                let names: Vec<_> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
                assert_eq!(names, ["private", "public", "remote"]);
                assert_eq!(rows[0]["hidden"], true);
                assert_eq!(rows[0]["shown"], true);
                assert_eq!(rows[0]["properties"]["quiet"], true);
                assert_eq!(rows[0]["properties"]["history"], 3);
                assert_eq!(rows[2]["readers"], json!([]), "public metadata survives an unavailable local project");
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn listing_only_walks_active_caches_when_size_is_requested() {
        for kind in [FolderIn::Cache, FolderIn::Versioned, FolderIn::Other] {
            for is_layer in [false, true] {
                for with_size in [false, true] {
                    let walks = std::cell::Cell::new(0);
                    let size = listing_size(with_size, kind, is_layer, || {
                        walks.set(walks.get() + 1);
                        11
                    });
                    let should_walk = with_size && kind == FolderIn::Cache && is_layer;
                    assert_eq!(walks.get(), u32::from(should_walk), "{kind:?}, layer={is_layer}, --size={with_size}");
                    assert_eq!(size, should_walk.then_some(11));
                }
            }
        }
    }

    #[test]
    fn listing_json_distinguishes_uncomputed_and_empty_sizes() -> Result<()> {
        let mut row = ListingRow {
            name: "cache.target".into(),
            mode: "follow".into(),
            hidden: false,
            owner: 1000,
            readers: Vec::new(),
            shown: false,
            properties: json!({}),
            folder: Some("target".into()),
            state: Some("cache".into()),
            size: None,
            saved: None,
        };
        assert!(serde_json::to_value(&row)?.get("size").is_none());
        row.size = Some(0);
        assert_eq!(serde_json::to_value(&row)?["size"], 0);
        row.size = Some(123);
        assert_eq!(serde_json::to_value(&row)?["size"], 123);
        Ok(())
    }

    #[test]
    fn listing_settings_reads_public_and_hidden_sources() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-list-settings-{}", ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let parent = crate::db::Db::open(&dir.join("parent.sqlite"))?;
        let overlay = crate::db::Db::open(&dir.join("overlay.sqlite"))?;
        for (db, key, value, ord) in [
            (&parent, "overlay.public.history", "2", "1"),
            (&parent, "overlay.public.history@machine", "4", "2"),
            (&parent, "overlay.public.folder", "\"target\"", "1"),
            (&parent, "overlay.private.history", "99", "1"),
            (&overlay, "overlay.private.history", "3", "1"),
            (&overlay, "overlay.private.quiet", "true", "1"),
        ] {
            db.conn.execute("INSERT INTO config (key, value, ord) VALUES (?1, ?2, ?3)", (key, value, ord))?;
        }
        let (public, folder) = listing_settings(&parent, "public")?;
        assert_eq!(public.history, Some(4));
        assert_eq!(folder.as_deref(), Some("target"));
        let (private, folder) = listing_settings(&overlay, "private")?;
        assert_eq!(private.history, Some(3));
        assert!(private.quiet);
        assert_eq!(folder, None);
        drop((parent, overlay));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn operation_output_only_contains_public_fields_and_cli_names() {
        for (signed, plain) in [("commit", "state"), ("rebase.step", "replay.step"), ("overlay.key", "overlay.key")] {
            let mut r = Record::new(signed);
            r.data = json!({"identity":"secret","recipient":"key material"});
            assert_eq!(operation_detail(&r), json!({"id":r.id,"op":plain,"actor":r.actor,"time":r.time}));
        }
    }

    #[test]
    fn reader_details_identify_principals_without_key_material() {
        for (principal, kind) in [("user:1000", "user"), ("group:1000", "group"), ("*", "everyone"), ("machine:id", "machine")] {
            assert_eq!(reader_detail(principal), json!({"principal":principal,"kind":kind}));
        }
        assert_eq!(
            reader_detail("person:reader@example.com=age1recipient"),
            json!({"principal":"person:reader@example.com","kind":"person"})
        );
    }
}
