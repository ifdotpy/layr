//! `layr project move <project> <volume>`: move a project to another volume (docs/volumes.md).
//! `layr project delete <project>`: remove a project without lines.
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::project_move;
use anyhow::{bail, Result};

const USAGE: &str = "usage: layr project move <project> <volume>\n       layr project delete <project>";

pub fn project(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let (cmd, rest) = args.split_first().map(|(c, r)| (c.as_str(), r)).unwrap_or(("", &[]));
    match cmd {
        "move" => {
            let a = parse(&Spec::new(), rest)?;
            let [project, volume] = a.pos.as_slice() else { bail!(USAGE) };
            let moved = project_move::move_project(&ctx.store, project, volume)?;
            outln!(
                ctx,
                "moved project {project} from volume {} to {} ({} states, {} lines, {} overlays)",
                moved.from,
                moved.to,
                moved.copy.states,
                moved.copy.lines,
                moved.copy.overlays
            );
            Ok(0)
        }
        "delete" => {
            let a = parse(&Spec::new().flag("-q|--quiet"), rest)?;
            let [project] = a.pos.as_slice() else { bail!(USAGE) };
            if project.contains('%') {
                bail!("'{project}' is an overlay; it goes with its project (layr overlay remove removes a cache overlay)");
            }
            if !crate::repo::Repo::open(ctx, project)?.is_admin(ctx) {
                bail!("permission denied: only an admin of project '{project}' deletes it");
            }
            project_move::delete_project(&ctx.store, project)?;
            if !a.has("-q") {
                outln!(ctx, "deleted project {project}");
            }
            Ok(0)
        }
        _ => bail!(USAGE),
    }
}
