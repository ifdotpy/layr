use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use layr_object_format::{Entry, Id, Keying, Kind, Node, Tree};

fn hashes(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash");
    let mut scratch = vec![0; 64 << 10];
    for (name, keying) in [("plain", Keying::Plain), ("keyed", Keying::Keyed([7; 32]))] {
        for size in [0, 4096, 1 << 20] {
            let data = vec![42; size];
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::new(name, size), &data, |b, bytes| {
                b.iter(|| {
                    let mut reader = black_box(bytes.as_slice());
                    keying.hash_reader(Kind::Blob, bytes.len() as u64, &mut reader, &mut scratch)
                });
            });
        }
    }
    group.finish();
}

fn trees(c: &mut Criterion) {
    let tree = Tree {
        entries: (0..1000)
            .map(|i| Entry {
                name: format!("file-{i:04}").into_bytes(),
                mode: 0o644,
                node: Node::File { size: 4096, blob: Id([3; 32]) },
                xattrs: vec![],
            })
            .collect(),
        links: vec![],
    };
    let bytes = tree.encode();
    c.bench_function("tree/encode_1000", |b| b.iter(|| black_box(&tree).encode()));
    c.bench_function("tree/decode_1000", |b| b.iter(|| Tree::decode(black_box(&bytes))));
}
criterion_group!(benches, hashes, trees);
criterion_main!(benches);
