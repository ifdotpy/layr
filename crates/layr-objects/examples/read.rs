//! Read speed of stored blobs (old versions, packs, delta chains).
//!
//! ```text
//! objects-read <store> sample <trees-file> <count>   random blobs of the listed trees, one read each
//! objects-read <store> decode <tree>                 every blob of one tree, in parallel
//! objects-read <store> phases <tree> <times>          the steps of a save that do not depend on the change
//! ```
use anyhow::Result;
use layr_objects::tree::Node;
use layr_objects::{Id, Keying, Store};
use rayon::prelude::*;
use std::path::Path;
use std::time::Instant;

fn blobs(store: &Store, tree: &Id, out: &mut Vec<(Id, u64)>) -> Result<()> {
    for e in store.get_tree(tree)?.entries {
        match e.node {
            Node::File { size, blob } => out.push((blob, size)),
            Node::Dir { tree } => blobs(store, &tree, out)?,
            Node::Symlink { .. } => {}
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let store = Store::open(Path::new(&a[0]), Keying::Plain)?;
    match a[1].as_str() {
        "sample" => {
            let mut all = Vec::new();
            for line in std::fs::read_to_string(&a[2])?.lines().filter(|l| !l.is_empty()) {
                blobs(&store, &Id::parse(line.trim())?, &mut all)?;
            }
            all.sort();
            all.dedup();
            let n: usize = a[3].parse()?;
            // A fixed pseudo-random sample, so runs compare. Each read opens the store anew with an
            // empty block cache, as a separate command does; the form is asked after the timer.
            let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
            let (mut opens, mut reads) = (Vec::new(), Vec::new());
            let mut bytes = 0u64;
            let mut forms = std::collections::BTreeMap::new();
            let root = Path::new(&a[0]);
            for _ in 0..n.min(all.len()) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let (id, _) = all[(x % all.len() as u64) as usize];
                let t = Instant::now();
                let fresh = Store::open(root, Keying::Plain)?;
                opens.push(t.elapsed().as_secs_f64() * 1000.0);
                let t = Instant::now();
                let data = fresh.read_blob(&id)?;
                reads.push(t.elapsed().as_secs_f64() * 1000.0);
                bytes += data.len() as u64;
                *forms.entry(format!("{:?}", fresh.form(&id)?).split(['(', ' ', '{']).next().unwrap_or("").to_string()).or_insert(0) += 1;
            }
            let show = |name: &str, v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p = |q: f64| v[((v.len() as f64 - 1.0) * q) as usize];
                println!("{name}: median {:.2} ms, p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms", p(0.5), p(0.9), p(0.99), p(1.0));
            };
            println!("{} distinct blobs; {} cold reads, {} bytes; forms {:?}", all.len(), reads.len(), bytes, forms);
            show("open the store", &mut opens);
            show("read one blob", &mut reads);
        }
        "decode" => {
            let mut all = Vec::new();
            blobs(&store, &Id::parse(&a[2])?, &mut all)?;
            let t = Instant::now();
            let total: u64 = all.par_iter().map(|(id, _)| store.read_blob(id).map(|d| d.len() as u64).unwrap_or(0)).sum();
            let s = t.elapsed().as_secs_f64();
            println!("decoded {} blobs, {} bytes in {:.2} s: {:.0} MB/s", all.len(), total, s, total as f64 / 1e6 / s);
        }
        "phases" => {
            let (tree, times) = (Id::parse(&a[2])?, Id::parse(&a[3])?);
            let t = Instant::now();
            let root = store.get_tree(&tree)?;
            println!("get root tree: {:.2} ms ({} entries)", t.elapsed().as_secs_f64() * 1000.0, root.entries.len());
            let t = Instant::now();
            let mut table = store.get_times(&times)?;
            println!("read and decode the root times: {:.2} ms ({} records)", t.elapsed().as_secs_f64() * 1000.0, table.records.len());
            let t = Instant::now();
            let all = store.times_ids(&times)?;
            println!("walk all times tables: {:.2} ms ({} tables)", t.elapsed().as_secs_f64() * 1000.0, all.len());
            table.own += 1;
            let t = Instant::now();
            store.put_times(&table)?;
            println!("hash, compress, write the root times: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        _ => {}
    }
    Ok(())
}
