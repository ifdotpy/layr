//! Read speed of stored blobs (old versions, packs, delta chains).
//!
//! ```text
//! objects-read <store> sample <trees-file> <count> [--state <id>]
//! objects-read <store> decode <tree> [--state <id>]
//! objects-read <store> phases <tree> <times>          the steps of a save that do not depend on the change
//! ```
//! `sample` chooses distinct blobs with a fixed seed. Each read uses a new object-store cache;
//! the operating system cache can stay warm. `--state` requires the named local state to reference
//! the measured tree (every listed tree for `sample`).
use anyhow::{bail, ensure, Context, Result};
use layr_objects::tree::Node;
use layr_objects::{Id, Keying, Store};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
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

fn sample_indices(len: usize, count: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..len).collect();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 0..count.min(len) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = i + (x % (len - i) as u64) as usize;
        indices.swap(i, j);
    }
    indices.truncate(count.min(len));
    indices
}

fn decoded_bytes(store: &Store, all: &[(Id, u64)]) -> Result<u64> {
    let lengths: Result<Vec<u64>> = all
        .par_iter()
        .map(|(id, expected)| {
            let len = store.read_blob(id)?.len() as u64;
            ensure!(len == *expected, "blob {} has size {len}, expected {expected}", id.hex());
            Ok(len)
        })
        .collect();
    lengths?.into_iter().try_fold(0u64, |total, len| total.checked_add(len).context("decoded byte count overflows"))
}

fn run(a: &[String]) -> Result<()> {
    let (a, state) =
        if a.len() >= 2 && a[a.len() - 2] == "--state" { (&a[..a.len() - 2], Some(a[a.len() - 1].as_str())) } else { (a, None) };
    let usage = "usage: objects-read <store> sample <trees-file> <count> [--state <id>] | decode <tree> [--state <id>] | phases <tree> <times>";
    match a.get(1).map(String::as_str) {
        Some("sample") if a.len() == 4 => {}
        Some("decode") if a.len() == 3 => {}
        Some("phases") if a.len() == 4 && state.is_none() => {}
        _ => bail!("{usage}"),
    }
    let root = Path::new(&a[0]);
    ensure!(root.join("config.json").is_file(), "object store does not exist: {}", root.display());
    let config = std::fs::read_to_string(root.join("config.json"))?;
    let store = Store::open(root, Keying::Plain)?;
    let state_tree = state
        .map(|id| -> Result<Id> {
            let found = store.state_ref(id)?.with_context(|| format!("unknown object-store state {id}"))?;
            ensure!(store.has(&found.tree) && store.has(&found.times), "objects of state {id} are missing");
            Ok(found.tree)
        })
        .transpose()?;
    println!("object store format: {}", config.trim());
    if let Some(id) = state {
        println!("state: {id}");
    }
    match a[1].as_str() {
        "sample" => {
            let mut all = Vec::new();
            let tree_list = std::fs::read(&a[2])?;
            let text = std::str::from_utf8(&tree_list).context("tree list is not UTF-8")?;
            let mut tree_count = 0;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let tree = Id::parse(line.trim())?;
                ensure!(state_tree.is_none_or(|expected| expected == tree), "tree does not match state");
                tree_count += 1;
                blobs(&store, &tree, &mut all)?;
            }
            ensure!(state.is_none() || tree_count > 0, "a state needs a tree in the input list");
            all.sort();
            for pair in all.windows(2) {
                ensure!(pair[0].0 != pair[1].0 || pair[0].1 == pair[1].1, "one blob has conflicting sizes");
            }
            all.dedup_by_key(|entry| entry.0);
            let n: usize = a[3].parse()?;
            println!(
                "tree list sha256: {}; {} input trees",
                hex::encode(Sha256::digest(&tree_list)),
                text.lines().filter(|l| !l.trim().is_empty()).count()
            );
            // The fixed sample has no repeated blobs. Each read starts with an empty block cache.
            let (mut opens, mut reads) = (Vec::new(), Vec::new());
            let mut bytes = 0u64;
            let mut forms = std::collections::BTreeMap::new();
            for index in sample_indices(all.len(), n) {
                let (id, expected) = all[index];
                let t = Instant::now();
                let fresh = Store::open(root, Keying::Plain)?;
                opens.push(t.elapsed().as_secs_f64() * 1000.0);
                let t = Instant::now();
                let data = fresh.read_blob(&id)?;
                ensure!(data.len() as u64 == expected, "blob {} has an unexpected size", id.hex());
                reads.push(t.elapsed().as_secs_f64() * 1000.0);
                bytes = bytes.checked_add(data.len() as u64).context("sample byte count overflows")?;
                *forms
                    .entry(format!("{:?}", fresh.form(&id)?).split(['(', ' ', '{']).next().unwrap_or("").to_string())
                    .or_insert(0) += 1;
            }
            let show = |name: &str, v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p = |q: f64| v[((v.len() as f64 - 1.0) * q) as usize];
                println!("{name}: median {:.2} ms, p90 {:.2} ms, p99 {:.2} ms, max {:.2} ms", p(0.5), p(0.9), p(0.99), p(1.0));
            };
            println!("{} distinct blobs; {} independent reads, {} bytes; forms {:?}", all.len(), reads.len(), bytes, forms);
            if reads.is_empty() {
                println!("no reads to measure");
                return Ok(());
            }
            show("open the store", &mut opens);
            show("read one blob", &mut reads);
        }
        "decode" => {
            let mut all = Vec::new();
            let tree = Id::parse(&a[2])?;
            ensure!(state_tree.is_none_or(|expected| expected == tree), "tree does not match state");
            blobs(&store, &tree, &mut all)?;
            println!("tree: {}", tree.hex());
            if all.is_empty() {
                println!("no blobs to decode");
                return Ok(());
            }
            let t = Instant::now();
            let total = decoded_bytes(&store, &all)?;
            let s = t.elapsed().as_secs_f64();
            ensure!(s > 0.0, "the elapsed time is too short to measure");
            println!("decoded {} blobs, {} bytes in {:.6} s: {:.3} MB/s", all.len(), total, s, total as f64 / 1e6 / s);
        }
        "phases" => {
            let (tree, times) = (Id::parse(&a[2])?, Id::parse(&a[3])?);
            println!("tree: {}; times: {}", tree.hex(), times.hex());
            let t = Instant::now();
            let root = store.get_tree(&tree)?;
            println!("get root tree: {:.2} ms ({} entries)", t.elapsed().as_secs_f64() * 1000.0, root.entries.len());
            let t = Instant::now();
            let mut table = store.get_times(&times)?;
            println!(
                "read and decode the root times: {:.2} ms ({} records)",
                t.elapsed().as_secs_f64() * 1000.0,
                table.records.len()
            );
            let t = Instant::now();
            let all = store.times_ids(&times)?;
            println!("walk all times tables: {:.2} ms ({} tables)", t.elapsed().as_secs_f64() * 1000.0, all.len());
            table.own += 1;
            let t = Instant::now();
            store.put_times(&table)?;
            println!("hash, compress, write the root times: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn main() -> Result<()> {
    run(&std::env::args().skip(1).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use layr_objects::tree::{Entry, Tree};

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "layr-objects-read-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sample_is_reproducible_and_does_not_repeat_blobs() {
        let sample = sample_indices(50, 30);
        assert_eq!(sample, sample_indices(50, 30));
        assert_eq!(sample.iter().copied().collect::<std::collections::BTreeSet<_>>().len(), 30);
        assert!(sample_indices(0, 1).is_empty());
        assert!(sample_indices(5, 0).is_empty());
    }

    #[test]
    fn empty_inputs_are_valid_and_missing_arguments_do_not_make_a_store() {
        let temp = Temp::new();
        let root = temp.0.join("store");
        assert!(run(&[root.display().to_string(), "decode".into()]).is_err());
        assert!(!root.exists());
        let store = Store::open(&root, Keying::Plain).unwrap();
        let tree = store.put_tree(&Tree::default()).unwrap();
        let trees = temp.0.join("trees");
        std::fs::write(&trees, "").unwrap();
        run(&[root.display().to_string(), "sample".into(), trees.display().to_string(), "1".into()]).unwrap();
        run(&[root.display().to_string(), "decode".into(), tree.hex()]).unwrap();
    }

    #[test]
    fn missing_blobs_are_errors_not_zero_bytes() {
        let temp = Temp::new();
        let store = Store::open(&temp.0.join("store"), Keying::Plain).unwrap();
        let missing = Id::parse(&"00".repeat(32)).unwrap();
        let tree = store
            .put_tree(&Tree {
                entries: vec![Entry {
                    name: b"missing".to_vec(),
                    mode: 0o644,
                    node: Node::File { size: 1, blob: missing },
                    xattrs: Vec::new(),
                }],
                links: Vec::new(),
            })
            .unwrap();
        let mut all = Vec::new();
        blobs(&store, &tree, &mut all).unwrap();
        assert!(decoded_bytes(&store, &all).is_err());
        assert!(run(&[store.root().display().to_string(), "decode".into(), tree.hex()]).is_err());
    }

    #[test]
    fn a_blob_with_a_different_size_is_an_error() {
        let temp = Temp::new();
        let store = Store::open(&temp.0.join("store"), Keying::Plain).unwrap();
        let source = temp.0.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), b"abc").unwrap();
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let tree = layr_objects::write_tree(&store, &source, &options).unwrap().tree.unwrap();
        let mut all = Vec::new();
        blobs(&store, &tree, &mut all).unwrap();
        assert_eq!(decoded_bytes(&store, &all).unwrap(), 3);
        all[0].1 += 1;
        assert!(decoded_bytes(&store, &all).is_err());
    }

    #[test]
    fn reported_state_must_match_the_measured_tree() {
        let temp = Temp::new();
        let store = Store::open(&temp.0.join("store"), Keying::Plain).unwrap();
        let tree = store.put_tree(&Tree::default()).unwrap();
        let times = store.put_times(&layr_objects::times::Times { own: 0, records: Vec::new() }).unwrap();
        store.set_state("test-state", &layr_objects::StateRef { tree, times }).unwrap();
        let root = store.root().display().to_string();
        run(&[root.clone(), "decode".into(), tree.hex(), "--state".into(), "test-state".into()]).unwrap();
        let trees = temp.0.join("trees");
        std::fs::write(&trees, format!("{}\n", tree.hex())).unwrap();
        run(&[root.clone(), "sample".into(), trees.display().to_string(), "1".into(), "--state".into(), "test-state".into()])
            .unwrap();
        let wrong = "00".repeat(32);
        assert!(run(&[root, "decode".into(), wrong, "--state".into(), "test-state".into()]).is_err());
    }
}
