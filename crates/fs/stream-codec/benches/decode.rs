use criterion::{Criterion, Throughput};
use layr_stream_codec::{encode_command, CommandHeader, DecodeError, DecodeLimits, MetadataCommand, Version, MAGIC};
use std::hint::black_box;

fn decode(c: &mut Criterion) -> Result<(), DecodeError> {
    let limits = DecodeLimits::default();
    let mut stream = MAGIC.to_vec();
    stream.extend(1u32.to_le_bytes());
    for i in 0u64..50_000 {
        let path = format!("dir/file-{i:05}");
        stream.extend(encode_command(3, &[(15, path.as_bytes()), (3, &i.to_le_bytes())], limits)?);
        stream.extend(encode_command(
            22,
            &[(15, path.as_bytes()), (18, &0u64.to_le_bytes()), (4, &4096u64.to_le_bytes())],
            limits,
        )?);
    }
    let mut group = c.benchmark_group("metadata");
    group.throughput(Throughput::Elements(50_000));
    let decode = |stream: &[u8]| -> Result<(), DecodeError> {
        let version = Version::decode(&stream[..17])?;
        let mut rest = &stream[17..];
        while !rest.is_empty() {
            let header = CommandHeader::decode(&rest[..10], limits)?;
            black_box(MetadataCommand::decode(header, &rest[10..10 + header.body_len()], version, limits)?);
            rest = &rest[10 + header.body_len()..];
        }
        Ok(())
    };
    decode(&stream)?;
    group.bench_function("decode_50k_entries", |b| b.iter(|| black_box(decode(black_box(&stream)))));
    group.finish();
    Ok(())
}
fn main() -> Result<(), DecodeError> {
    let mut criterion = Criterion::default().configure_from_args();
    decode(&mut criterion)?;
    criterion.final_summary();
    Ok(())
}
