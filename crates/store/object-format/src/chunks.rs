//! Content-defined chunks of cache files (FastCDC with normalized chunking).
//!
//! The cut points are part of the store format: the same bytes must give the same chunks in
//! every layr version, or chunks of an older push stop matching. So the gear table and the
//! masks below are fixed, and a test pins the cut points of a known input.

/// No chunk is shorter, except the last chunk of a file.
pub const MIN: usize = 16 << 10;
/// The size that the masks aim at.
pub const AVG: usize = 64 << 10;
/// No chunk is longer.
pub const MAX: usize = 256 << 10;

/// Before `AVG` bytes a cut needs 18 zero bits (rarer), after it 14 (more likely). This keeps
/// most chunks near `AVG`. The bits are the high bits of the hash, which depend on the last 64
/// bytes; the low bits of a shifting gear hash depend on only a few bytes.
const MASK_SMALL: u64 = !0 << (64 - 18);
const MASK_LARGE: u64 = !0 << (64 - 14);

/// splitmix64 from seed 0: a fixed table that anyone can recompute.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = 0u64;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// The length of the first chunk of `data`.
fn cut(data: &[u8]) -> usize {
    if data.len() <= MIN {
        return data.len();
    }
    let end = data.len().min(MAX);
    let normal = AVG.min(end);
    let mut hash = 0u64;
    let mut i = MIN;
    while i < normal {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_SMALL == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < end {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_LARGE == 0 {
            return i + 1;
        }
        i += 1;
    }
    end
}

/// The chunks of `data` in order. An empty input has no chunks.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(cut(rest));
        out.push(chunk);
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn chunks_cover_the_input_within_bounds() {
        for len in [0, 1, MIN - 1, MIN, MIN + 1, AVG, MAX, MAX + 1, 3 * MAX + 17, 4 << 20] {
            let data = noise(len, len as u64);
            let chunks = split(&data);
            assert_eq!(chunks.concat(), data);
            for (i, chunk) in chunks.iter().enumerate() {
                assert!(chunk.len() <= MAX);
                assert!(chunk.len() >= MIN || i + 1 == chunks.len(), "short chunk inside the input");
            }
        }
    }

    #[test]
    fn cut_points_are_fixed() {
        // A change here changes the store format: chunks of older pushes would stop matching.
        let lengths: Vec<usize> = split(&noise(1 << 20, 7)).iter().map(|c| c.len()).collect();
        assert_eq!(GEAR[0], 0xe220_a839_7b1d_cdaf);
        assert_eq!(lengths.iter().sum::<usize>(), 1 << 20);
        assert_eq!(lengths, PINNED.to_vec());
    }
    const PINNED: &[usize] =
        &[83931, 66477, 104326, 58501, 72892, 66658, 66748, 81136, 67277, 68591, 82002, 117972, 99038, 13027];

    #[test]
    fn an_insertion_changes_only_nearby_chunks() {
        let data = noise(2 << 20, 11);
        let mut changed = data.clone();
        changed.splice(1 << 20..1 << 20, b"inserted bytes".iter().copied());
        let before: std::collections::BTreeSet<&[u8]> = split(&data).into_iter().collect();
        let after = split(&changed);
        let new = after.iter().filter(|c| !before.contains(*c)).count();
        assert!(new <= 2, "{new} of {} chunks changed", after.len());
    }
}
