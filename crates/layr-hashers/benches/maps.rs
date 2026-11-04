use rustc_hash::FxHasher;
use std::{
    collections::HashMap,
    hash::{BuildHasher, BuildHasherDefault, Hash, Hasher},
    hint::black_box,
    time::Instant,
};

fn test<K: Hash + Eq + Clone, B: BuildHasher + Clone>(label: &str, shape: &str, keys: &[K], build: B) {
    let mut buckets = vec![0u32; (keys.len() * 2).next_power_of_two()];
    let mask = buckets.len() - 1;
    for key in keys {
        let mut h = build.build_hasher();
        key.hash(&mut h);
        buckets[(h.finish() as usize) & mask] += 1;
    }
    let collisions: u64 = buckets.iter().map(|&n| u64::from(n.saturating_sub(1))).sum();
    let max = buckets.iter().copied().max().unwrap_or(0);
    let start = Instant::now();
    let mut map = HashMap::with_capacity_and_hasher(keys.len(), build);
    for (i, k) in keys.iter().enumerate() {
        map.insert(k.clone(), i);
    }
    let insert = start.elapsed();
    let start = Instant::now();
    for k in keys {
        black_box(map.get(black_box(k)));
    }
    println!(
        "{shape},{},{label},{:.3},{:.3},{collisions},{max}",
        keys.len(),
        insert.as_secs_f64() * 1000.,
        start.elapsed().as_secs_f64() * 1000.
    );
}
fn all<K: Hash + Eq + Clone>(shape: &str, keys: &[K]) {
    test("std", shape, keys, std::collections::hash_map::RandomState::new());
    test("fx", shape, keys, BuildHasherDefault::<FxHasher>::default());
    test("rapid", shape, keys, rapidhash::fast::RandomState::default());
    test("fold", shape, keys, foldhash::fast::RandomState::default());
}
fn main() {
    println!("shape,count,hasher,insert_ms,lookup_ms,bucket_collisions,max_bucket");
    for n in [1000, 100_000, 1_000_000] {
        let uuid = |i: usize| {
            format!(
                "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
                i.wrapping_mul(2654435761) as u32,
                i & 0xffff,
                i & 0xfff,
                (i >> 12) & 0xfff,
                i
            )
        };
        let strings: Vec<_> = (0..n).map(uuid).collect();
        all("uuid", &strings);
        let paths: Vec<_> =
            (0..n).map(|i| format!("src/{:04}/module-{}/{}file-{i}.rs", i % 1009, i % 113, "segment/".repeat(i % 12))).collect();
        all("path", &paths);
        let ints: Vec<u64> = (0..n as u64).collect();
        all("u64", &ints);
        let tuples: Vec<_> = strings.into_iter().zip(paths).collect();
        all("tuple", &tuples);
    }
}
