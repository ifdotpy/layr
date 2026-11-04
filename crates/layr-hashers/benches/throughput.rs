use layr_hashers::{ContentAddress, FastCryptographic, FastNonCryptographic, GitCompatible, PurposeHasher};
use std::{
    hash::{BuildHasher, Hasher},
    hint::black_box,
    time::{Duration, Instant},
};

fn measure(mut f: impl FnMut(), size: usize) -> f64 {
    let start = Instant::now();
    let mut count = 0u64;
    while start.elapsed() < Duration::from_millis(150) {
        f();
        count += 1;
    }
    (count as f64 * size as f64) / start.elapsed().as_secs_f64() / 1_000_000.0
}
#[inline(never)]
fn generic<H: PurposeHasher>(data: &[u8]) -> H::Output {
    H::hash(data)
}
fn run<H: PurposeHasher>(name: &str, data: &[u8]) {
    let one = measure(
        || {
            black_box(generic::<H>(black_box(data)));
        },
        data.len(),
    );
    let stream = measure(
        || {
            let mut h = H::new();
            for part in black_box(data).chunks(4096) {
                h.update(part);
            }
            black_box(h.finish());
        },
        data.len(),
    );
    println!("{name},{},{one:.1},{stream:.1}", data.len());
}
fn main() {
    for size in [8, 64, 4096, 1 << 20] {
        alternatives(&vec![42; size]);
    }
    println!("hasher,bytes,one-shot MB/s,stream MB/s");
    for size in [64, 4096, 1 << 20, 100 << 20] {
        let data = vec![42; size];
        run::<GitCompatible>("git", &data);
        run::<ContentAddress>("content", &data);
        run::<FastCryptographic>("fast-crypto", &data);
        run::<FastNonCryptographic>("fast-noncrypto", &data);
        let direct = measure(
            || {
                black_box(blake3::hash(black_box(&data)));
            },
            size,
        );
        let generic = measure(
            || {
                black_box(generic::<FastCryptographic>(black_box(&data)));
            },
            size,
        );
        let standard = measure(
            || {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                h.write(black_box(&data));
                black_box(h.finish());
            },
            size,
        );
        println!("compare,{size},blake3-direct={direct:.1},blake3-generic={generic:.1},default-hasher={standard:.1}");
    }
}

#[allow(dead_code)]
fn alternatives(data: &[u8]) {
    let size = data.len();
    let rapid = measure(
        || {
            black_box(rapidhash::v3::rapidhash_v3(black_box(data)));
        },
        size,
    );
    let fold = measure(
        || {
            let mut h = foldhash::fast::FixedState::default().build_hasher();
            h.write(black_box(data));
            black_box(h.finish());
        },
        size,
    );
    let xxh = measure(
        || {
            black_box(xxhash_rust::xxh3::xxh3_64(black_box(data)));
        },
        size,
    );
    let standard = measure(
        || {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            h.write(black_box(data));
            black_box(h.finish());
        },
        size,
    );
    println!("alternatives,{size},rapid={rapid:.1},fold={fold:.1},xxh3={xxh:.1},default={standard:.1}");
}
