//! `layr volume`: the file systems that hold projects (docs/volumes.md).
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::volumes::{self, MAIN};
use anyhow::{bail, Result};
use std::path::Path;

pub fn volume(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let (cmd, rest) = args.split_first().map(|(c, r)| (c.as_str(), r)).unwrap_or(("list", &[]));
    let root = ctx.store.root.as_path();
    let machine = ctx.store.machine.id.as_str();
    match cmd {
        "add" => {
            let a = parse(&Spec::new(), rest)?;
            let [name, path] = a.pos.as_slice() else { bail!("usage: layr volume add <name> <path>") };
            let path = volumes::add(root, machine, name, Path::new(path), backend)?;
            outln!(ctx, "added volume {name} at {}", path.display());
            Ok(0)
        }
        "list" => {
            let a = parse(&Spec::new().flag("--json"), rest)?;
            if !a.pos.is_empty() {
                bail!("usage: layr volume list [--json]");
            }
            list(ctx, a.has("--json"))
        }
        "remove" => {
            let a = parse(&Spec::new(), rest)?;
            let [name] = a.pos.as_slice() else { bail!("usage: layr volume remove <name>") };
            volumes::remove(root, machine, name)?;
            outln!(ctx, "removed volume {name}");
            Ok(0)
        }
        "default" => {
            let a = parse(&Spec::new(), rest)?;
            let [name] = a.pos.as_slice() else { bail!("usage: layr volume default <name>") };
            volumes::set_default(root, machine, name)?;
            outln!(ctx, "default volume: {name}");
            Ok(0)
        }
        other => bail!("unknown volume command '{other}'; use add, list, remove or default"),
    }
}

/// The file system a volume must have for the build: APFS on macOS, btrfs (not in the root
/// subvolume) on Linux, ext4 or XFS for the OverlayFS build.
fn backend(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if !crate::storage::is_supported(path) {
        bail!("{} is not on APFS; a volume of this build needs APFS", path.display());
    }
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        if !crate::storage::is_supported(path) {
            bail!("{} is not on btrfs; a volume of this build needs btrfs", path.display());
        }
        crate::rootcheck::check_store(path, &[])?;
    }
    #[cfg(feature = "overlayfs")]
    {
        let fs = volumes::fs_type(path);
        if fs != "ext4" && fs != "xfs" {
            bail!("{} is on {fs}; a volume of the OverlayFS build needs ext4 or XFS", path.display());
        }
    }
    Ok(())
}

/// One volume of `layr volume list`.
struct Row {
    name: String,
    path: std::path::PathBuf,
    fs: String,
    used: Option<u64>,
    free: Option<u64>,
    projects: usize,
    default: bool,
    status: String,
}

/// The volumes with their file system, space, project count and state.
fn rows(ctx: &Ctx) -> Result<Vec<Row>> {
    let layout = ctx.store.layout()?;
    let mut out = Vec::new();
    for name in layout.names() {
        let path = layout.path_of(&name).unwrap_or_default();
        let (status, usable) = match layout.ready(&name) {
            Ok(p) => (String::from("ok"), Some(p)),
            Err(e) => (format!("{e:#}"), None),
        };
        let (used, free, projects) = match &usable {
            Some(p) => {
                let (used, free) = volumes::space(p).map(|(u, f)| (Some(u), Some(f))).unwrap_or((None, None));
                let projects = std::fs::read_dir(layout.projects_folder(&name)?)
                    .map(|d| d.flatten().filter(|e| crate::store::db_file(&e.path()).is_file()).count())
                    .unwrap_or(0);
                (used, free, projects)
            }
            None => (None, None, layout.projects_on(&name).len()),
        };
        out.push(Row {
            fs: volumes::fs_type(&path),
            default: layout.default_volume() == name,
            name,
            path,
            used,
            free,
            projects,
            status,
        });
    }
    Ok(out)
}

fn list(ctx: &Ctx, json: bool) -> Result<i32> {
    let rows = rows(ctx)?;
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "volume": r.name,
                    "path": r.path,
                    "fs": r.fs,
                    "used": r.used,
                    "free": r.free,
                    "projects": r.projects,
                    "default": r.default,
                    "status": r.status,
                })
            })
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&v)?);
        return Ok(0);
    }
    let size = |b: Option<u64>| b.map(super::admin::human_size).unwrap_or_else(|| "-".into());
    let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(MAIN.len());
    outln!(ctx, "{:<width$}  {:<8} {:>10} {:>10} {:>9}  {}", "volume", "fs", "used", "free", "projects", "path");
    for r in &rows {
        let name = if r.default { format!("{}*", r.name) } else { r.name.clone() };
        outln!(
            ctx,
            "{:<width$}  {:<8} {:>10} {:>10} {:>9}  {}",
            name,
            r.fs,
            size(r.used),
            size(r.free),
            r.projects,
            r.path.display()
        );
        if r.status != "ok" {
            outln!(ctx, "\t{}", r.status);
        }
    }
    outln!(ctx, "* default volume for new projects");
    Ok(0)
}
