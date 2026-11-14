//! Overlay project creation and its initial access policy.

use crate::ctx::Ctx;
use crate::ids;
use crate::model::{StateKind, StateRec};
use crate::overlay::{Mode, Props, PROPS};
use crate::repo::Repo;
use crate::storage as btrfs;
use crate::store::Project;
use anyhow::Result;
use std::collections::BTreeMap;

/// Make overlay `name` of the caller with its empty first state and its records. The caller holds
/// the project lock and has checked that no overlay has the name. `folder`: a folder overlay;
/// `cache`: the cache overlay of a cache path (`save --overlays`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_overlay(
    locked: &crate::store::LockedProject<'_>,
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
        locked.check(&repo.p)?;
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
    let made = (|| -> crate::db::PublishResult<String> {
        let orepo = Repo { p: op, ctx: Some(ctx) };
        let overlay_lock = orepo.p.lock_maintenance()?;
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
        let mut rec = crate::model::DraftRecord::new("commit");
        rec.states.push(st);
        orepo.commit_record(&overlay_lock, ctx, rec)?;
        let mut rec = crate::model::DraftRecord::new("overlay.create");
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
            orepo.commit_record(&overlay_lock, ctx, rec)?;
        } else {
            repo.commit_record(locked, ctx, rec)?;
        }
        Ok(id)
    })();
    if let Err(e) = made {
        // No record of the project names the overlay: its folder goes.
        if matches!(e, crate::db::CommitResolution::NotCommitted(_)) && repo.overlay_info(name)?.is_none() {
            let _ = btrfs::delete_tree(&dir);
        }
        return Err(e.into());
    }
    Ok(())
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
