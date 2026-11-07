//! Measure the object store on a plain folder (docs/objects.md, "Measurements").
//!
//! ```text
//! cargo run --release -p layr-objects --example objects-bench -- <store> import <folder> [hot|cold]
//! cargo run --release -p layr-objects --example objects-bench -- <store> checkout <tree> <folder>
//! cargo run --release -p layr-objects --example objects-bench -- <store> verify <original> <copy>
//! cargo run --release -p layr-objects --example objects-bench -- <store> stat
//! ```
use anyhow::{bail, Result};
use layr_objects::{checkout, compare, write_tree, Id, Keying, Store, Temperature, WriteOptions};
use std::path::Path;
use std::time::Instant;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let store = Store::open(Path::new(&args[0]), Keying::Plain)?;
    let started = Instant::now();
    match args.get(1).map(String::as_str) {
        Some("import") => {
            let temperature = match args.get(3).map(String::as_str) {
                Some("cold") => Temperature::Cold,
                _ => Temperature::Hot,
            };
            let previous = args.get(4).map(|t| Id::parse(t)).transpose()?;
            let r = write_tree(
                &store,
                Path::new(&args[2]),
                &WriteOptions { temperature, previous, exclude: vec![b".git".to_vec()] },
            )?;
            println!(
                "tree {} files {} dirs {} symlinks {} bytes {} links {} new objects {} new bytes {} clones {} in {:.2}s",
                r.tree.unwrap(),
                r.files,
                r.dirs,
                r.symlinks,
                r.bytes,
                r.hard_link_groups,
                store.added.objects.load(std::sync::atomic::Ordering::Relaxed),
                store.added.bytes.load(std::sync::atomic::Ordering::Relaxed),
                store.added.clones.load(std::sync::atomic::Ordering::Relaxed),
                started.elapsed().as_secs_f64()
            );
        }
        Some("checkout") => {
            let r = checkout(&store, &Id::parse(&args[2])?, Path::new(&args[3]))?;
            println!("{r:?} in {:.2}s", started.elapsed().as_secs_f64());
        }
        Some("verify") => {
            let c = compare(Path::new(&args[2]), Path::new(&args[3]))?;
            println!(
                "entries {} bytes {} differences {} in {:.2}s",
                c.entries,
                c.bytes,
                c.differences.len(),
                started.elapsed().as_secs_f64()
            );
            for d in c.differences.iter().take(20) {
                println!("  {d}");
            }
        }
        Some("stat") => {
            let s = store.stat()?;
            println!("{s:#?}\ntotal bytes {} in {:.2}s", s.total_bytes(), started.elapsed().as_secs_f64());
        }
        _ => bail!("usage: objects-bench <store> import|checkout|verify|stat ..."),
    }
    Ok(())
}
