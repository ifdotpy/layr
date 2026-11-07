//! zstd frames with a chosen level, long-distance matching and an optional dictionary (pack
//! blocks; `layr_store::zst` makes the loose frames).

use anyhow::{anyhow, ensure, Result};
use zstd_safe::{CCtx, CParameter, DCtx};

fn error(code: usize) -> anyhow::Error {
    anyhow!("zstd: {}", zstd_safe::get_error_name(code))
}

/// How a block is compressed.
#[derive(Clone, Debug, Default)]
pub struct Codec {
    pub level: i32,
    /// Long-distance matching with a window that covers the input (at most 128 MiB).
    pub long: bool,
    pub dict: Option<Vec<u8>>,
}

pub fn compress(data: &[u8], codec: &Codec) -> Result<Vec<u8>> {
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(codec.level)).map_err(error)?;
    cctx.set_parameter(CParameter::ContentSizeFlag(true)).map_err(error)?;
    if codec.long {
        let window_log = (u64::BITS - (data.len() as u64).leading_zeros() + 1).clamp(10, 27);
        cctx.set_parameter(CParameter::WindowLog(window_log)).map_err(error)?;
        cctx.set_parameter(CParameter::EnableLongDistanceMatching(true)).map_err(error)?;
    }
    if let Some(dict) = &codec.dict {
        cctx.load_dictionary(dict).map_err(error)?;
    }
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(error)?;
    Ok(out)
}

/// Decode one frame of exactly `size` bytes.
pub fn decompress(frame: &[u8], dict: Option<&[u8]>, size: usize) -> Result<Vec<u8>> {
    let mut dctx = DCtx::create();
    dctx.set_parameter(zstd_safe::DParameter::WindowLogMax(27)).map_err(error)?;
    if let Some(dict) = dict {
        dctx.load_dictionary(dict).map_err(error)?;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(size)?;
    dctx.decompress(&mut out, frame).map_err(error)?;
    ensure!(out.len() == size, "a block decodes to {} bytes, its index says {size}", out.len());
    Ok(out)
}

/// A dictionary of at most `size` bytes trained on `samples` (zstd's ZDICT trainer).
pub fn train(samples: &[&[u8]], size: usize) -> Result<Vec<u8>> {
    let sizes: Vec<usize> = samples.iter().map(|s| s.len()).collect();
    let joined: Vec<u8> = samples.concat();
    let mut dict = Vec::with_capacity(size);
    zstd_safe::train_from_buffer(&mut dict, &joined, &sizes).map_err(error)?;
    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_each_codec() {
        let mut samples = Vec::new();
        for i in 0..400 {
            samples.push(format!("fn function_{i}(value: u64) -> u64 {{\n    value * {i} + {}\n}}\n", i * 7).into_bytes());
        }
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dict = train(&refs, 4096).unwrap();
        let data = samples[17].clone();
        for codec in [
            Codec { level: 3, long: false, dict: None },
            Codec { level: 19, long: true, dict: None },
            Codec { level: 19, long: false, dict: Some(dict.clone()) },
        ] {
            let frame = compress(&data, &codec).unwrap();
            assert_eq!(decompress(&frame, codec.dict.as_deref(), data.len()).unwrap(), data);
            assert!(decompress(&frame, codec.dict.as_deref(), data.len() + 1).is_err());
        }
        let with = compress(&data, &Codec { level: 19, long: false, dict: Some(dict) }).unwrap();
        let without = compress(&data, &Codec { level: 19, long: false, dict: None }).unwrap();
        assert!(with.len() < without.len(), "a trained dictionary helps a small file");
    }
}
