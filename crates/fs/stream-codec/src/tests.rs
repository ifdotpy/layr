#![allow(clippy::unwrap_used)]
use super::*;

fn version(n: u32) -> Version {
    let mut header = MAGIC.to_vec();
    header.extend(n.to_le_bytes());
    Version::decode(&header).unwrap()
}
fn frame(code: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    encode_command(code, attrs, DecodeLimits::default()).unwrap()
}
fn metadata(bytes: &[u8]) -> Result<MetadataCommand> {
    let limits = DecodeLimits::default();
    MetadataCommand::decode(CommandHeader::decode(&bytes[..10], limits)?, &bytes[10..], version(1), limits)
}
fn verify<'a>(decoder: &mut ReceiveDecoder, bytes: &'a [u8]) -> Result<VerifiedReceiveCommand<'a>> {
    decoder.decode(CommandHeader::decode(&bytes[..10], DecodeLimits::default())?, &bytes[10..])
}
fn subvol() -> Vec<u8> {
    frame(1, &[(15, b"state"), (1, &[1; 16]), (2, &1u64.to_le_bytes())])
}

#[test]
fn metadata_commands_and_ranges() {
    assert_eq!(metadata(&frame(3, &[(15, b"file")])), Ok(MetadataCommand::Create { path: "file".into(), kind: Kind::File }));
    assert_eq!(metadata(&frame(9, &[(15, b"a"), (16, b"b")])), Ok(MetadataCommand::Rename { from: "a".into(), to: "b".into() }));
    assert_eq!(
        metadata(&frame(10, &[(15, b"a"), (17, b"b")])),
        Ok(MetadataCommand::Link { path: "a".into(), target: "b".into() })
    );
    for code in [16, 22] {
        let attrs = [(15, b"file".as_slice()), (18, &8u64.to_le_bytes()), (4, &2u64.to_le_bytes()), (24, &2u64.to_le_bytes())];
        assert_eq!(
            metadata(&frame(code, &attrs)),
            Ok(MetadataCommand::Extent { path: "file".into(), offset: 8, end: RangeEnd::Offset(10) })
        );
        let attrs =
            [(15, b"file".as_slice()), (18, &u64::MAX.to_le_bytes()), (4, &1u64.to_le_bytes()), (24, &1u64.to_le_bytes())];
        assert_eq!(metadata(&frame(code, &attrs)), Err(DecodeError::Overflow));
    }
    for code in [15, 23, 25] {
        assert_eq!(
            metadata(&frame(code, &[(15, b"f"), (18, &8u64.to_le_bytes())])),
            Ok(MetadataCommand::Extent { path: "f".into(), offset: 8, end: RangeEnd::Eof })
        );
    }
    assert_eq!(metadata(&frame(21, &[])), Ok(MetadataCommand::Other));
    assert_eq!(metadata(&frame(99, &[])), Err(DecodeError::UnknownCommand(99)));
    assert_eq!(metadata(&frame(20, &[(15, b"bad\xff")])), Err(DecodeError::Path));
    for code in [19, 24] {
        assert_eq!(metadata(&frame(code, &[(15, b"file")])), Ok(MetadataCommand::Metadata { path: "file".into() }));
    }
}

#[test]
fn malformed_attributes_never_default_or_silently_end() {
    for len in 0..=16 {
        if len == 8 {
            continue;
        }
        assert_eq!(metadata(&frame(17, &[(15, b"f"), (4, &vec![0; len])])), Err(DecodeError::AttributeSize(4)));
    }
    for attrs in [vec![], vec![(15, b"f".as_slice())]] {
        assert!(matches!(metadata(&frame(17, &attrs)), Err(DecodeError::MissingAttribute(_))));
    }
    assert_eq!(metadata(&frame(3, &[(15, b"a"), (15, b"b")])), Err(DecodeError::DuplicateAttribute(15)));
    let valid = frame(17, &[(15, b"file"), (4, &10u64.to_le_bytes())]);
    for n in 0..valid.len() {
        let result = if n < 10 {
            CommandHeader::decode(&valid[..n], DecodeLimits::default()).map(|_| ())
        } else {
            metadata(&valid[..n]).map(|_| ())
        };
        assert!(result.is_err(), "prefix {n}");
    }
    for tail in [vec![1], vec![15, 0, 255, 255], vec![15, 0, 1]] {
        let mut bytes = frame(99, &[]);
        bytes[..4].copy_from_slice(&u32::try_from(tail.len()).unwrap().to_le_bytes());
        bytes.extend(tail);
        assert_eq!(metadata(&bytes), Err(DecodeError::Truncated));
    }
}

#[test]
fn limits_are_checked_before_collecting_or_allocating() {
    let mut header = [0; 10];
    header[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(CommandHeader::decode(&header, DecodeLimits::default()), Err(DecodeError::Limit)));
    let limits = DecodeLimits { max_attributes: 1, max_path_bytes: 2, ..DecodeLimits::default() };
    for bytes in [frame(3, &[(15, b"long")]), frame(3, &[(40, b"a"), (41, b"b")])] {
        let header = CommandHeader::decode(&bytes[..10], limits).unwrap();
        assert_eq!(MetadataCommand::decode(header, &bytes[10..], version(1), limits), Err(DecodeError::Limit));
    }
}

#[test]
fn strict_receive_checks_crc_paths_required_fields_and_order() {
    let good = frame(3, &[(15, b"f"), (3, &1u64.to_le_bytes())]);
    let mut decoder = ReceiveDecoder::new(version(1), DecodeLimits::default());
    assert!(verify(&mut decoder, &good).is_err());
    verify(&mut decoder, &subvol()).unwrap();
    for path in [b"../escape".as_slice(), b"a//b", b"/a", b"a/./b", b"a\0b"] {
        assert!(verify(&mut decoder, &frame(3, &[(15, path), (3, &1u64.to_le_bytes())])).is_err());
    }
    assert!(verify(&mut decoder, &frame(99, &[])).is_err());
    assert!(verify(&mut decoder, &frame(3, &[(15, b"f")])).is_err());
    assert!(verify(&mut decoder, &frame(1, &[(15, b"f"), (1, &[1; 16])])).is_err());
    let mut damaged = good.clone();
    damaged[10] ^= 1;
    assert!(matches!(verify(&mut decoder, &damaged), Err(DecodeError::Checksum)));
    verify(&mut decoder, &good).unwrap();
    assert!(verify(&mut decoder, &subvol()).is_err());
    verify(&mut decoder, &frame(21, &[])).unwrap();
    assert!(verify(&mut decoder, &good).is_err());
    decoder.finish().unwrap();
    assert!(ReceiveDecoder::new(version(1), DecodeLimits::default()).finish().is_err());
}

#[test]
fn version_two_data_takes_the_remaining_body() {
    let mut body = vec![15, 0, 1, 0, b'f', 18, 0, 8, 0];
    body.extend(0u64.to_le_bytes());
    body.extend([19, 0, 255, 0, 1, 2, 3]);
    let attrs = Attributes::decode(&body, version(2), DecodeLimits::default()).unwrap();
    assert_eq!(attrs.require(19).unwrap(), &[255, 0, 1, 2, 3]);
    assert!(Attributes::decode(&body, version(1), DecodeLimits::default()).is_err());
}

#[test]
fn deterministic_fuzz_style_mutations_do_not_panic() {
    let seed = frame(22, &[(15, b"a/b"), (18, &4u64.to_le_bytes()), (4, &8u64.to_le_bytes())]);
    for i in 0..seed.len() {
        for byte in [0, 1, 19, 127, 255] {
            let mut bytes = seed.clone();
            bytes[i] = byte;
            let _ = metadata(&bytes);
            let mut decoder = ReceiveDecoder::new(version(1), DecodeLimits::default());
            verify(&mut decoder, &subvol()).unwrap();
            let _ = verify(&mut decoder, &bytes);
        }
    }
}

#[test]
fn every_receive_command_has_a_valid_fixture_and_reports_each_missing_field() {
    let uuid = [2; 16];
    let num = 0u64.to_le_bytes();
    let mode = 0u32.to_le_bytes();
    let time = [0; 12];
    let attrs = [
        (1, uuid.as_slice()),
        (2, &num),
        (3, &num),
        (4, &num),
        (5, &num),
        (6, &num),
        (7, &num),
        (8, &num),
        (9, &time),
        (10, &time),
        (11, &time),
        (13, b"user.test"),
        (14, b"value"),
        (15, b"file"),
        (16, b"to"),
        (17, b"target"),
        (18, &num),
        (19, b"data"),
        (20, &uuid),
        (21, &num),
        (22, b"source"),
        (23, &num),
        (24, &num),
        (25, &mode),
        (26, &num),
        (27, &num),
        (28, &num),
        (29, &num),
        (32, &[1]),
        (33, &mode),
        (34, b"salt"),
        (35, b"sig"),
    ];
    // These fixtures describe the wire contract independently of the decoder's match arms.
    let required: &[&[u16]] = &[
        &[15, 1, 2],
        &[15, 1, 2, 20, 21],
        &[15, 3],
        &[15, 3],
        &[15, 3, 5, 8],
        &[15, 3],
        &[15, 3],
        &[15, 3, 17],
        &[15, 16],
        &[15, 17],
        &[15],
        &[15],
        &[15, 13, 14],
        &[15, 13],
        &[15, 18, 19],
        &[15, 18, 20, 21, 22, 23, 24],
        &[15, 4],
        &[15, 5],
        &[15, 6, 7],
        &[15, 9, 10, 11],
        &[],
        &[15, 18, 4],
        &[15, 25, 18, 4],
        &[15, 26],
        &[15, 18, 19, 27, 28, 29],
        &[15, 32, 33, 34, 35],
    ];
    for (index, &tags) in required.iter().enumerate() {
        let code = u16::try_from(index + 1).unwrap();
        let fields: Vec<_> = attrs.iter().copied().filter(|(tag, _)| tags.contains(tag)).collect();
        // Version 2+ data is last and omits its length. Other attributes retain v1 framing.
        let wire = |fields: &[(u16, &[u8])]| {
            let mut fields = fields.to_vec();
            fields.sort_by_key(|(tag, _)| *tag == 19);
            let mut bytes = frame(code, &fields);
            if fields.iter().any(|(tag, _)| *tag == 19) {
                let len = fields.last().unwrap().1.len();
                let offset = bytes.len() - len - 2;
                bytes.drain(offset..offset + 2);
                let len = u32::try_from(bytes.len() - 10).unwrap();
                bytes[..4].copy_from_slice(&len.to_le_bytes());
                bytes[6..10].fill(0);
                let crc = crc_parts(&bytes, &[]);
                bytes[6..10].copy_from_slice(&crc.to_le_bytes());
            }
            bytes
        };
        let mut decoder = ReceiveDecoder::new(version(3), DecodeLimits::default());
        if code > 2 {
            verify(&mut decoder, &subvol()).unwrap();
        }
        assert!(verify(&mut decoder, &wire(&fields)).is_ok(), "command {code}");
        for &missing in tags {
            let mut decoder = ReceiveDecoder::new(version(3), DecodeLimits::default());
            if code > 2 {
                verify(&mut decoder, &subvol()).unwrap();
            }
            let incomplete: Vec<_> = fields.iter().copied().filter(|(tag, _)| *tag != missing).collect();
            assert!(
                matches!(verify(&mut decoder, &wire(&incomplete)), Err(DecodeError::MissingAttribute(tag)) if tag == missing),
                "command {code}, missing {missing}"
            );
        }
    }
}

#[test]
fn end_command_matches_the_btrfs_crc32c_wire_vector() {
    // Independently computed with the reflected CRC32C polynomial and zero seed.
    let end = [0, 0, 0, 0, 21, 0, 0x50, 0x6c, 0xc9, 0x9d];
    assert_eq!(frame(21, &[]), end);
    let mut decoder = ReceiveDecoder::new(version(1), DecodeLimits::default());
    verify(&mut decoder, &subvol()).unwrap();
    verify(&mut decoder, &end).unwrap();
    decoder.finish().unwrap();
}
