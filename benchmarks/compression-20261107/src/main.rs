// Compression choices for the layr object store, measured on real files.
//   zbench files <list> <max-bytes>   one path per line
//   zbench delta <pairs>              "old<TAB>new" per line
use std::time::Instant;
use zstd::bulk::{Compressor, Decompressor};
use zstd::zstd_safe::CParameter;

fn read_list(p: &str, max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut total = 0;
    for line in std::fs::read_to_string(p).unwrap().lines() {
        if let Ok(d) = std::fs::read(line) {
            total += d.len();
            out.push(d);
            if total >= max { break; }
        }
    }
    out
}

fn mb(n: usize) -> f64 { n as f64 / 1048576.0 }

fn per_file(files: &[Vec<u8>], level: i32, dict: Option<&[u8]>) -> (usize, f64, f64) {
    let mut c = match dict { Some(d) => Compressor::with_dictionary(level, d).unwrap(), None => Compressor::new(level).unwrap() };
    let mut d = match dict { Some(x) => Decompressor::with_dictionary(x).unwrap(), None => Decompressor::new().unwrap() };
    let t = Instant::now();
    let packed: Vec<Vec<u8>> = files.iter().map(|f| c.compress(f).unwrap()).collect();
    let ct = t.elapsed().as_secs_f64();
    let t = Instant::now();
    for (p, f) in packed.iter().zip(files) { let o = d.decompress(p, f.len()).unwrap(); assert_eq!(o.len(), f.len()); }
    (packed.iter().map(|p| p.len()).sum(), ct, t.elapsed().as_secs_f64())
}

fn stream(all: &[u8], level: i32, long: bool) -> (usize, f64, f64) {
    let mut c = Compressor::new(level).unwrap();
    if long {
        c.set_parameter(CParameter::EnableLongDistanceMatching(true)).unwrap();
        c.set_parameter(CParameter::WindowLog(27)).unwrap();
    }
    let t = Instant::now();
    let p = c.compress(all).unwrap();
    let ct = t.elapsed().as_secs_f64();
    let mut d = Decompressor::new().unwrap();
    if long { d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(27)).unwrap(); }
    let t = Instant::now();
    let o = d.decompress(&p, all.len()).unwrap();
    assert_eq!(o.len(), all.len());
    (p.len(), ct, t.elapsed().as_secs_f64())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "files" => {
            let files = read_list(&a[2], a[3].parse().unwrap());
            let raw: usize = files.iter().map(|f| f.len()).sum();
            println!("files {}  raw {:.1} MiB", files.len(), mb(raw));
            println!("{:<26} {:>10} {:>7} {:>10} {:>10}", "variant", "MiB", "ratio", "comp MB/s", "dec MB/s");
            let row = |name: &str, (s, ct, dt): (usize, f64, f64), raw: usize| {
                println!("{:<26} {:>10.2} {:>6.1}% {:>10.1} {:>10.1}", name, mb(s), 100.0 * s as f64 / raw as f64, mb(raw) / ct, mb(raw) / dt);
            };
            for l in [3, 9, 19] { row(&format!("per file, level {l}"), per_file(&files, l, None), raw); }
            let all: Vec<u8> = files.concat();
            row("one stream, level 3", stream(&all, 3, false), raw);
            row("one stream, level 19", stream(&all, 19, false), raw);
            row("one stream, 19 + long", stream(&all, 19, true), raw);
            // Dictionary for small files.
            let small: Vec<Vec<u8>> = files.iter().filter(|f| f.len() < 65536 && !f.is_empty()).cloned().collect();
            let sraw: usize = small.iter().map(|f| f.len()).sum();
            let samples: Vec<&[u8]> = small.iter().take(20000).map(|f| f.as_slice()).collect();
            let t = Instant::now();
            let dict = zstd::dict::from_samples(&samples, 112 * 1024).unwrap();
            println!("small files (<64 KiB): {}  raw {:.1} MiB; dictionary {} KiB trained in {:.1} s", small.len(), mb(sraw), dict.len() / 1024, t.elapsed().as_secs_f64());
            for l in [3, 19] {
                row(&format!("small, level {l}"), per_file(&small, l, None), sraw);
                row(&format!("small, level {l} + dict"), per_file(&small, l, Some(&dict)), sraw);
            }
            // Cost of a save: compress 1 and 10 typical files.
            let mut sizes: Vec<&Vec<u8>> = files.iter().collect();
            sizes.sort_by_key(|f| f.len());
            let typical: Vec<Vec<u8>> = sizes[sizes.len() / 2..].iter().take(10).map(|f| (*f).clone()).collect();
            for l in [3, 19] {
                for n in [1, 10] {
                    let mut best = f64::MAX;
                    for _ in 0..20 {
                        let mut c = Compressor::new(l).unwrap();
                        let t = Instant::now();
                        for f in &typical[..n] { std::hint::black_box(c.compress(f).unwrap()); }
                        best = best.min(t.elapsed().as_secs_f64());
                    }
                    println!("save of {n:>2} files ({:.0} KiB each), level {l:>2}: {:.2} ms", mb(typical[0].len()) * 1024.0, best * 1000.0);
                }
            }
        }
        "delta" => {
            let mut whole = 0; let mut delta = 0; let mut raw = 0; let mut n = 0;
            for line in std::fs::read_to_string(&a[2]).unwrap().lines() {
                let Some((o, w)) = line.split_once('\t') else { continue };
                let (Ok(old), Ok(new)) = (std::fs::read(o), std::fs::read(w)) else { continue };
                raw += new.len(); n += 1;
                whole += Compressor::new(19).unwrap().compress(&new).unwrap().len();
                delta += Compressor::with_dictionary(19, &old).unwrap().compress(&new).unwrap().len();
            }
            println!("pairs {n}  raw {:.2} MiB  whole zstd19 {:.3} MiB  delta zstd19 {:.3} MiB", mb(raw), mb(whole), mb(delta));
        }
        _ => {}
    }
}
