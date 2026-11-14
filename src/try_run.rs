//! Private execution copies and the identity/environment of a try command.

use crate::ctx::Ctx;
use crate::errors::{error, Kind as ErrorKind};
use crate::ids;
use crate::model::Line;
use crate::repo::Repo;
use crate::storage as btrfs;
use anyhow::Result;
use std::path::Path;

/// Create a private try copy, run the callback, then remove the copy unless requested otherwise.
pub fn with_try_copy<T>(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    layers: &[crate::overlay::Layer],
    keep: bool,
    caches: bool,
    run: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let name = format!(".try-{:016x}", rand::random::<u64>());
    let dir = repo.p.line_path(&name);
    let tmp_line = Line {
        id: format!("try-{name}"),
        name,
        head: state.into(),
        owner: ctx.caller.uid,
        machine: repo.p.store.machine.id.clone(),
    };
    if layers.is_empty() {
        repo.materialize(&tmp_line, state, &dir)?;
    } else {
        let view = crate::overlay::View::new(state.to_string(), layers.to_vec());
        repo.materialize_view(ctx, &tmp_line, &view, &dir)?;
    }
    if caches {
        // A check starts warm: the cache of the state, by the rules of `line new <state>`. A plain fetch takes only
        // the records of a cache overlay, so the cache comes now from the store remote.
        crate::store_remote::fetch_caches_on_demand(ctx, repo, state, None, false);
        if let Err(e) = repo.copy_caches(ctx, &tmp_line, state, &dir) {
            let _ = btrfs::delete_tree(&dir);
            return Err(e);
        }
    }
    let result = run(&dir);
    if keep {
        eprintln!("layr try: kept {}", dir.display());
    } else {
        let _ = btrfs::delete_tree(&dir);
    }
    result
}

/// Use the try identity and environment rules for commands from an exact tree.
pub fn try_command(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    argv: &[String],
    dir: &Path,
    env: &[(String, String)],
    json: bool,
) -> Result<i32> {
    if argv.is_empty() || argv[0].is_empty() {
        anyhow::bail!("a command needs a nonempty argv");
    }
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(dir);
    if json {
        command.stdout(std::process::Stdio::from(rustix::io::dup(std::io::stderr())?));
    }
    let mut env = env.to_vec();
    env.push(("LAYR_TRY_STATE".into(), ids::display(state)));
    env.retain(|(k, _)| k != "LAYR_PROJECT");
    env.push(("LAYR_PROJECT".into(), repo.p.name.clone()));
    crate::privs::command_as(&mut command, ctx.caller.uid, &env)?;
    let status = command.status().map_err(|e| {
        error(ErrorKind::CannotRun, format!("layr try: cannot run '{}' as user {}: {e}", argv[0], ctx.caller.user))
    })?;
    Ok(status.code().unwrap_or(128))
}
