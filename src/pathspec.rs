/// A simple glob: `*` and `?` (both match `/` too, as in git pathspecs) and `[...]`.
/// Iterative, with one backtrack point (the last `*`): time O(pattern * text), no recursion.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    // One pattern item at p[i] against byte c: Some(length of the item) if it matches.
    let item = |i: usize, c: u8| -> Option<usize> {
        match p[i] {
            b'?' => Some(1),
            b'[' => match p[i + 1..].iter().position(|x| *x == b']') {
                Some(off) => {
                    let end = i + 1 + off;
                    let set = &p[i + 1..end];
                    let (neg, set) =
                        if set.first() == Some(&b'!') || set.first() == Some(&b'^') { (true, &set[1..]) } else { (false, set) };
                    let mut hit = false;
                    let mut k = 0;
                    while k < set.len() {
                        if k + 2 < set.len() && set[k + 1] == b'-' {
                            hit |= c >= set[k] && c <= set[k + 2];
                            k += 3;
                        } else {
                            hit |= c == set[k];
                            k += 1;
                        }
                    }
                    (hit != neg).then_some(end + 1 - i)
                }
                None => (c == b'[').then_some(1),
            },
            x => (x == c).then_some(1),
        }
    };
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < s.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some((i, j));
            i += 1;
            continue;
        }
        if i < p.len() {
            if let Some(n) = item(i, s[j]) {
                i += n;
                j += 1;
                continue;
            }
        }
        match star {
            // Let the last `*` take one more byte and try again from there.
            Some((si, sj)) => {
                i = si + 1;
                j = sj + 1;
                star = Some((si, sj + 1));
            }
            None => return false,
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

/// True if `path` matches one of the pathspecs (empty list matches everything).
pub fn matches(specs: &[String], path: &str) -> bool {
    if specs.is_empty() {
        return true;
    }
    specs.iter().any(|s| {
        s.is_empty() || path == s || path.starts_with(&format!("{s}/")) || (s.contains(['*', '?', '[']) && glob(s, path))
    })
}
