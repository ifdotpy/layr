//! layrfs-fuse: the helpers of the FUSE view benchmarks (docs/layrfs-fuse.md). They make the
//! inputs of the benchmarks and the saves of the Btrfs and OverlayFS backends with the same
//! layr-objects code that layr uses. The view itself is `layr fs view` (feature `layrfs-fuse`).

use anyhow::{bail, Context, Result};
use layr_objects::write::{write_changed, Base};
use layr_objects::{Keying, StateRef, Store, Temperature, WriteOptions};
use std::path::Path;
use std::time::Instant;

const USAGE: &str = "usage:
  layrfs-fuse import <folder> <objects> <state>           write a folder as a hot state
  layrfs-fuse checkout <objects> <state> <folder>         a native folder of a state (clones)
  layrfs-fuse write-changed <objects> <base> <root> <paths-file> <state>
                                                          save the listed paths of a native folder
  layrfs-fuse ls-tree <objects> <state>                   the root entries of a state: name, mode, id
";

fn main() {
    if let Err(e) = run() {
        eprintln!("layrfs-fuse: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = args.split_first().context(USAGE)?;
    match cmd.as_str() {
        "import" => import(rest),
        "checkout" => checkout(rest),
        "write-changed" => changed(rest),
        "ls-tree" => ls_tree(rest),
        _ => bail!("{USAGE}"),
    }
}

fn open_store(dir: &str) -> Result<Store> {
    Store::open(Path::new(dir), Keying::Plain)
}

fn import(a: &[String]) -> Result<()> {
    let [folder, objects, state] = a else { bail!("{USAGE}") };
    let t = Instant::now();
    let store = open_store(objects)?;
    let options = WriteOptions { temperature: Temperature::Hot, previous: None, exclude: Vec::new(), similar: false };
    let r = layr_objects::write_tree(&store, Path::new(folder), &options)?;
    let (tree, times) = (r.tree.context("no tree")?, r.times.context("no times")?);
    store.set_state(state, &StateRef { tree, times })?;
    println!(
        "{{\"state\":\"{state}\",\"tree\":\"{tree}\",\"files\":{},\"folders\":{},\"bytes\":{},\"seconds\":{:.3}}}",
        r.files,
        r.dirs,
        r.bytes,
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

fn checkout(a: &[String]) -> Result<()> {
    let [objects, state, folder] = a else { bail!("{USAGE}") };
    let store = open_store(objects)?;
    let r = store.state_ref(state)?.context("no such state")?;
    let t = Instant::now();
    layr_objects::checkout_with(&store, &r.tree, Some(&r.times), Path::new(folder))?;
    println!("{{\"seconds\":{:.3}}}", t.elapsed().as_secs_f64());
    Ok(())
}

fn ls_tree(a: &[String]) -> Result<()> {
    let [objects, state] = a else { bail!("{USAGE}") };
    let store = open_store(objects)?;
    let r = store.state_ref(state)?.context("no such state")?;
    for e in store.get_tree(&r.tree)?.entries {
        let id = match &e.node {
            layr_objects::Node::File { blob, .. } => blob.hex(),
            layr_objects::Node::Dir { tree } => tree.hex(),
            layr_objects::Node::Symlink { target } => String::from_utf8_lossy(target).into_owned(),
        };
        println!("{} {:o} {id}", String::from_utf8_lossy(&e.name), e.mode);
    }
    Ok(())
}

/// The object write of a save on the Btrfs and OverlayFS backends: the change list comes from
/// elsewhere (`btrfs send --no-data`, a scan of the upper folder).
fn changed(a: &[String]) -> Result<()> {
    let [objects, base, root, list, state] = a else { bail!("{USAGE}") };
    let store = open_store(objects)?;
    let b = store.state_ref(base)?.context("no base state")?;
    let paths: Vec<Vec<u8>> = std::fs::read(list)?.split(|c| *c == b'\n').filter(|l| !l.is_empty()).map(|l| l.to_vec()).collect();
    let t = Instant::now();
    let options =
        WriteOptions { temperature: Temperature::Hot, previous: None, exclude: vec![b"target".to_vec()], similar: false };
    let r = write_changed(&store, Path::new(root), &Base { tree: b.tree, times: b.times }, &paths, &options)?;
    let (tree, times) = (r.tree.context("no tree")?, r.times.context("no times")?);
    store.set_state(state, &StateRef { tree, times })?;
    println!(
        "{{\"state\":\"{state}\",\"tree\":\"{tree}\",\"paths\":{},\"seconds\":{:.4}}}",
        paths.len(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}
