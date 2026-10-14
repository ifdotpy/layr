//! Local macOS execution. No privileged multi-user daemon is exposed.
use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};
pub fn daemon(_: &[String]) -> Result<i32> {
    bail!("the APFS backend has no privileged daemon; run commands directly")
}
pub fn forward(_: &[String]) -> Result<i32> {
    bail!("the APFS backend runs locally")
}
pub fn run_as(argv: Vec<String>, caller: Caller, cwd: PathBuf, env: HashMap<String, String>, root: &Path) -> i32 {
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot open the layr store {}: {e:#}", root.display());
            return 128;
        }
    };
    let out: Box<dyn Write> = Box::new(std::io::BufWriter::new(std::io::stdout()));
    let ctx = Ctx { store, caller, cwd, env, out: std::cell::RefCell::new(out) };
    let r = crate::cmd::run(&ctx, &argv);
    let _ = ctx.out.borrow_mut().flush();
    match r {
        Ok(c) => c,
        Err(e) => {
            if let Some(x) = e.downcast_ref::<crate::cmd::Exit>() {
                if !x.1.is_empty() {
                    eprintln!("{}", x.1);
                }
                x.0
            } else {
                eprintln!("fatal: {e:#}");
                128
            }
        }
    }
}
