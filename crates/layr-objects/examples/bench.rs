//! Measure the object store on plain folders (docs/objects.md, "Measurements").
//!
//! ```text
//! objects-bench <store> import <folder> hot|cold [--previous <tree>] [--similar] [--exclude a,b]
//! objects-bench <store> changed <folder> <tree> <times> <path>... [--exclude a,b]
//! objects-bench <store> checkout <tree> <folder> [--times <id>]
//! objects-bench <store> verify <original> <copy> [--times]
//! objects-bench <store> repack [--level N] [--no-long] [--no-dict] [--block BYTES]
//! objects-bench <store> matrix [--block BYTES]
//! objects-bench <store> stat
//! objects-bench - dictgain <folder>...
//! ```
//! `cargo run --release -p layr-objects --example objects-bench -- ...`
use anyhow::{bail, Result};
use layr_objects::write::{write_changed, Base};
use layr_objects::zstdx::{self, Codec};
use layr_objects::{checkout_with, compare_with, write_tree, Id, Keying, Store, Temperature, WriteOptions};
use rayon::prelude::*;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The dictionary size of the measurements (zstd's default for `--train`).
const DICT_SIZE: usize = 112 << 10;

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn mb_s(bytes: u64, seconds: f64) -> f64 {
    bytes as f64 / (1 << 20) as f64 / seconds
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.get(1).map(String::as_str) == Some("dictgain") {
        return dictgain(&args[2..]);
    }
    let mut store = Store::open(Path::new(&args[0]), Keying::Plain)?;
    let started = Instant::now();
    let rest = &args[2..];
    match args.get(1).map(String::as_str) {
        Some("import") => {
            let temperature = if rest.get(1).map(String::as_str) == Some("cold") { Temperature::Cold } else { Temperature::Hot };
            let previous = value(rest, "--previous").map(Id::parse).transpose()?;
            let exclude = value(rest, "--exclude")
                .map_or(vec![b".git".to_vec()], |x| x.split(',').map(|s| s.as_bytes().to_vec()).collect());
            let opts = WriteOptions { temperature, previous, exclude, similar: flag(rest, "--similar") };
            let r = write_tree(&store, Path::new(&rest[0]), &opts)?;
            println!(
                "tree {} times {} files {} dirs {} bytes {} new objects {} new bytes {} clones {} in {:.3}s",
                r.tree.unwrap(),
                r.times.unwrap(),
                r.files,
                r.dirs,
                r.bytes,
                store.added.objects.load(Ordering::Relaxed),
                store.added.bytes.load(Ordering::Relaxed),
                store.added.clones.load(Ordering::Relaxed),
                started.elapsed().as_secs_f64()
            );
        }
        Some("changed") => {
            let base = Base { tree: Id::parse(&rest[1])?, times: Id::parse(&rest[2])? };
            let exclude = value(rest, "--exclude")
                .map_or(vec![b".git".to_vec()], |x| x.split(',').map(|s| s.as_bytes().to_vec()).collect());
            let changed: Vec<Vec<u8>> =
                rest[3..].iter().take_while(|p| *p != "--exclude").map(|p| p.as_bytes().to_vec()).collect();
            let opts = WriteOptions { temperature: Temperature::Hot, previous: None, exclude, similar: false };
            let r = write_changed(&store, Path::new(&rest[0]), &base, &changed, &opts)?;
            println!(
                "tree {} times {} files read {} new objects {} in {:.2} ms",
                r.tree.unwrap(),
                r.times.unwrap(),
                r.files,
                store.added.objects.load(Ordering::Relaxed),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        Some("checkout") => {
            let times = value(rest, "--times").map(Id::parse).transpose()?;
            let r = checkout_with(&store, &Id::parse(&rest[0])?, times.as_ref(), Path::new(&rest[1]))?;
            println!("{r:?} in {:.2}s", started.elapsed().as_secs_f64());
        }
        Some("verify") => {
            let c = compare_with(Path::new(&rest[0]), Path::new(&rest[1]), flag(rest, "--times"))?;
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
        Some("repack") => {
            store.options.pack_level = value(rest, "--level").map_or(Ok(19), str::parse)?;
            store.options.pack_long = !flag(rest, "--no-long");
            store.options.train_dict = !flag(rest, "--no-dict");
            if let Some(b) = value(rest, "--block") {
                store.options.block_size = b.parse()?;
            }
            let r = store.pack()?;
            println!("{r:?} in {:.2}s", started.elapsed().as_secs_f64());
        }
        Some("matrix") => matrix(
            &store,
            value(rest, "--block").map_or(Ok(1 << 20), str::parse)?,
            value(rest, "--levels"),
            flag(rest, "--no-dict"),
        )?,
        Some("stat") => {
            let s = store.stat()?;
            println!("{s:#?}\ntotal bytes {} in {:.2}s", s.total_bytes(), started.elapsed().as_secs_f64());
        }
        _ => bail!("usage: see the top of examples/bench.rs"),
    }
    Ok(())
}

/// Compress the loose objects of a store as a repack would (pack order, plain blocks), with each
/// level, with and without a dictionary trained on small files. Wall times use all cores.
fn matrix(store: &Store, block: usize, levels: Option<&str>, no_dict: bool) -> Result<()> {
    let t = Instant::now();
    let (order, blocks) = store.plain_blocks(block)?;
    let plain: u64 = blocks.iter().map(|b| b.len() as u64).sum();
    let samples = store.small_blobs(&order, 16 << 20)?;
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    let dict = zstdx::train(&refs, DICT_SIZE)?;
    println!(
        "objects {} blocks {} of {} bytes, plain {} bytes, dictionary {} bytes from {} files, threads {}, prepared in {:.1}s",
        order.len(),
        blocks.len(),
        block,
        plain,
        dict.len(),
        samples.len(),
        rayon::current_num_threads(),
        t.elapsed().as_secs_f64()
    );
    println!("| variant | dictionary | bytes | ratio | compress MB/s | decompress MB/s |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let wanted: Vec<&str> = levels.map_or(vec!["3", "9", "19", "19+long"], |l| l.split(',').collect());
    for (name, level, long) in [("3", 3, false), ("9", 9, false), ("19", 19, false), ("19+long", 19, true)] {
        if !wanted.contains(&name) {
            continue;
        }
        for with_dict in [false, true] {
            if with_dict && no_dict {
                continue;
            }
            let codec = Codec { level, long, dict: with_dict.then(|| dict.clone()) };
            let t = Instant::now();
            let frames = blocks.par_iter().map(|b| zstdx::compress(b, &codec)).collect::<Result<Vec<_>>>()?;
            let c = t.elapsed().as_secs_f64();
            let size: u64 = frames.iter().map(|f| f.len() as u64).sum::<u64>() + if with_dict { dict.len() as u64 } else { 0 };
            let t = Instant::now();
            frames
                .par_iter()
                .zip(blocks.par_iter())
                .try_for_each(|(f, b)| zstdx::decompress(f, codec.dict.as_deref(), b.len()).map(|_| ()))?;
            let d = t.elapsed().as_secs_f64();
            println!(
                "| {name} | {} | {size} | {:.3} | {:.0} | {:.0} |",
                if with_dict { "yes" } else { "no" },
                size as f64 / plain as f64,
                mb_s(plain, c),
                mb_s(plain, d)
            );
        }
    }
    Ok(())
}

/// The dictionary gain on small source files, each a frame of its own (the loose form) and in
/// 1 MiB blocks (the pack form). The dictionary learns from every other file and is measured on
/// the rest, so it never saw the files it compresses.
fn dictgain(folders: &[String]) -> Result<()> {
    let mut files: Vec<Vec<u8>> = Vec::new();
    for folder in folders {
        let mut stack = vec![std::path::PathBuf::from(folder)];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                let name = e.file_name();
                let ft = e.file_type()?;
                if ft.is_dir() && ![".git", "target", "node_modules", ".layr-retest"].contains(&name.to_string_lossy().as_ref()) {
                    stack.push(e.path());
                } else if ft.is_file() {
                    let len = e.metadata()?.len();
                    if len > 0 && len <= 16 << 10 {
                        files.push(std::fs::read(e.path())?);
                    }
                }
            }
        }
    }
    let refs: Vec<&[u8]> = files.iter().step_by(2).map(Vec::as_slice).collect();
    let dict = zstdx::train(&refs, DICT_SIZE)?;
    let test: Vec<&Vec<u8>> = files.iter().skip(1).step_by(2).collect();
    let plain: usize = test.iter().map(|f| f.len()).sum();
    println!("small files {} (test {} files, {} bytes), dictionary {} bytes", files.len(), test.len(), plain, dict.len());
    for level in [3, 19] {
        let frames = |dict: Option<&Vec<u8>>| -> Result<usize> {
            let codec = Codec { level, long: false, dict: dict.cloned() };
            Ok(test.par_iter().map(|f| zstdx::compress(f, &codec).map(|c| c.len())).collect::<Result<Vec<_>>>()?.iter().sum())
        };
        let blocks = |dict: Option<&Vec<u8>>| -> Result<usize> {
            let codec = Codec { level, long: false, dict: dict.cloned() };
            let mut out = Vec::new();
            let mut cur = Vec::new();
            for f in &test {
                if cur.len() + f.len() > 1 << 20 && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur.extend_from_slice(f);
            }
            out.push(cur);
            Ok(out.par_iter().map(|b| zstdx::compress(b, &codec).map(|c| c.len())).collect::<Result<Vec<_>>>()?.iter().sum())
        };
        let (fa, fb) = (frames(None)?, frames(Some(&dict))?);
        let (ba, bb) = (blocks(None)?, blocks(Some(&dict))?);
        println!(
            "level {level}: per-file frames {fa} -> {fb} bytes with the dictionary ({:.1} % saved); 1 MiB blocks {ba} -> {bb} ({:.1} % saved)",
            100.0 * (fa as f64 - fb as f64) / fa as f64,
            100.0 * (ba as f64 - bb as f64) / ba as f64
        );
    }
    Ok(())
}
