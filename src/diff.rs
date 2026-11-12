//! Diff output in git formats: patches, `--stat`, `--numstat`, `--name-status`, `--check`.

use crate::changes::Change;
use crate::fsutil::{self, FType, Meta};
use anyhow::Result;
use similar::DiffOp;

pub use layr_model::diff::{patch, FileDiff, Side};
pub use layr_model::diff::{patch_analyzed, AnalyzedDiff};

/// The bytes of the files of one side of a comparison: a folder tree, or content held in memory.
pub trait Contents {
    fn bytes(&self, path: &str) -> Result<Vec<u8>>;
}

impl<T: fsutil::Src + ?Sized> Contents for T {
    fn bytes(&self, path: &str) -> Result<Vec<u8>> {
        fsutil::entry_bytes(self, path)
    }
}

pub fn side_of(root: &(impl Contents + ?Sized), path: &str, meta: &Meta) -> Result<Side> {
    let big = (meta.ftype == FType::File && meta.size > fsutil::MAX_TEXT).then_some(meta.size);
    let data = match meta.ftype {
        _ if big.is_some() => Vec::new(),
        FType::File | FType::Symlink => root.bytes(path)?,
        _ => Vec::new(),
    };
    Ok(Side { path: path.to_string(), mode: meta.git_mode(), data, big })
}

/// Build file diffs for a change list between two roots.
pub fn file_diffs(
    old_root: &(impl Contents + ?Sized),
    new_root: &(impl Contents + ?Sized),
    changes: &[Change],
) -> Result<Vec<FileDiff>> {
    let mut v = Vec::new();
    for c in changes {
        let old = match &c.old {
            Some(m) if m.ftype != FType::Dir => Some(side_of(old_root, c.src(), m)?),
            _ => None,
        };
        let new = match &c.new {
            Some(m) if m.ftype != FType::Dir => Some(side_of(new_root, &c.path, m)?),
            _ => None,
        };
        if old.is_none() && new.is_none() {
            continue;
        }
        v.push(FileDiff { old, new });
    }
    Ok(v)
}

fn decimal_width(n: usize) -> usize {
    n.to_string().len()
}

fn scale(it: usize, width: usize, max: usize) -> usize {
    if it == 0 {
        return 0;
    }
    if max == 0 {
        return 0;
    }
    1 + (it * (width.saturating_sub(1))) / max
}

/// `--stat` output (git's layout for an 80 column width).
pub fn stat(files: &[FileDiff], width: usize) -> String {
    stat_analyzed(&files.iter().map(AnalyzedDiff::new).collect::<Vec<_>>(), width)
}

pub fn stat_analyzed(files: &[AnalyzedDiff<'_>], width: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    struct Row {
        name: String,
        add: usize,
        del: usize,
        bin: Option<(usize, usize)>,
    }
    let rows: Vec<Row> = files
        .iter()
        .map(|f| {
            let (a, d) = f.counts();
            let bin = if f.is_binary() {
                Some((f.old.as_ref().map(|s| s.len()).unwrap_or(0), f.new.as_ref().map(|s| s.len()).unwrap_or(0)))
            } else {
                None
            };
            Row { name: f.display_path(), add: a, del: d, bin }
        })
        .collect();
    let max_len = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0);
    let max_change = rows.iter().map(|r| r.add + r.del).max().unwrap_or(0);
    let mut number_width = decimal_width(max_change);
    if rows.iter().any(|r| r.bin.is_some()) && number_width < 3 {
        number_width = 3;
    }
    let mut name_width = max_len;
    let mut graph_width = max_change;
    if name_width + number_width + 6 + graph_width > width {
        let lim = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > lim {
            graph_width = lim.max(6);
        }
        if name_width > width.saturating_sub(number_width + 6 + graph_width) {
            name_width = width.saturating_sub(number_width + 6 + graph_width);
        } else {
            graph_width = width.saturating_sub(number_width + 6 + name_width);
        }
    }
    let mut s = String::new();
    let (mut ta, mut td) = (0, 0);
    for r in &rows {
        let mut name = r.name.clone();
        let n = name.chars().count();
        if n > name_width {
            let keep = name_width.saturating_sub(3);
            let tail: String = name.chars().skip(n - keep).collect();
            name = format!("...{tail}");
        }
        let pad = name_width.saturating_sub(name.chars().count());
        if let Some((a, b)) = r.bin {
            s.push_str(&format!(" {}{} | {:>w$} {} -> {} bytes\n", name, " ".repeat(pad), "Bin", a, b, w = number_width));
            continue;
        }
        ta += r.add;
        td += r.del;
        let total = r.add + r.del;
        let (a, d) = if max_change > graph_width {
            let total_scaled = scale(total, graph_width, max_change);
            let a = scale(r.add, graph_width, max_change);
            let d = total_scaled.saturating_sub(a);
            (a, d)
        } else {
            (r.add, r.del)
        };
        s.push_str(&format!(
            " {}{} | {:>w$}{}{}{}\n",
            name,
            " ".repeat(pad),
            total,
            if total > 0 { " " } else { "" },
            "+".repeat(a),
            "-".repeat(d),
            w = number_width
        ));
    }
    s.push_str(&summary(rows.len(), ta, td));
    s
}

pub fn summary(files: usize, add: usize, del: usize) -> String {
    let mut s = format!(" {} file{} changed", files, if files == 1 { "" } else { "s" });
    if add > 0 || del == 0 {
        s.push_str(&format!(", {} insertion{}(+)", add, if add == 1 { "" } else { "s" }));
    }
    if del > 0 || add == 0 {
        s.push_str(&format!(", {} deletion{}(-)", del, if del == 1 { "" } else { "s" }));
    }
    s.push('\n');
    s
}

pub fn shortstat(files: &[FileDiff]) -> String {
    shortstat_analyzed(&files.iter().map(AnalyzedDiff::new).collect::<Vec<_>>())
}

pub fn shortstat_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let (a, d) = line_totals(files);
    summary(files.len(), a, d)
}

/// The inserted and deleted lines of all files.
fn line_totals(files: &[AnalyzedDiff<'_>]) -> (usize, usize) {
    files.iter().map(AnalyzedDiff::counts).fold((0, 0), |(a, d), (x, y)| (a + x, d + y))
}

/// The numbers of `shortstat`: changed files, inserted lines and deleted lines.
pub fn totals(files: &[FileDiff]) -> (usize, usize, usize) {
    let (a, d) = line_totals(&files.iter().map(AnalyzedDiff::new).collect::<Vec<_>>());
    (files.len(), a, d)
}

pub fn numstat_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    let mut s = String::new();
    for f in files {
        if f.is_binary() {
            s.push_str(&format!("-\t-\t{}\n", f.display_path()));
        } else {
            let (a, d) = f.counts();
            s.push_str(&format!("{a}\t{d}\t{}\n", f.display_path()));
        }
    }
    s
}

pub fn name_status_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    let mut s = String::new();
    for f in files {
        match f.status() {
            'R' => s.push_str(&format!(
                "R{:03}\t{}\t{}\n",
                f.similarity(),
                f.old.as_ref().unwrap().path,
                f.new.as_ref().unwrap().path
            )),
            c => s.push_str(&format!("{c}\t{}\n", f.path())),
        }
    }
    s
}

pub fn name_only_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    files.iter().map(|f| format!("{}\n", f.path())).collect()
}

/// The `-z` forms, as git prints them: paths end with NUL and are never quoted; a rename gives
/// its old and new path as two fields (`numstat`: `<a>\t<d>\t\0<old>\0<new>\0`).
pub fn numstat_z_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    let mut s = String::new();
    for f in files {
        let counts = if f.is_binary() {
            "-\t-".to_string()
        } else {
            let (a, d) = f.counts();
            format!("{a}\t{d}")
        };
        match f.status() {
            'R' => s.push_str(&format!("{counts}\t\0{}\0{}\0", f.old.as_ref().unwrap().path, f.new.as_ref().unwrap().path)),
            _ => s.push_str(&format!("{counts}\t{}\0", f.path())),
        }
    }
    s
}

pub fn name_status_z_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    let mut s = String::new();
    for f in files {
        match f.status() {
            'R' => s.push_str(&format!(
                "R{:03}\0{}\0{}\0",
                f.similarity(),
                f.old.as_ref().unwrap().path,
                f.new.as_ref().unwrap().path
            )),
            c => s.push_str(&format!("{c}\0{}\0", f.path())),
        }
    }
    s
}

pub fn name_only_z_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    files.iter().map(|f| format!("{}\0", f.path())).collect()
}

/// Whitespace errors and conflict markers in added lines (`--check`).
pub fn check_analyzed(files: &[AnalyzedDiff<'_>]) -> String {
    let mut s = String::new();
    for f in files {
        if f.is_binary() {
            continue;
        }
        if f.new.is_none() {
            continue;
        }
        let edits = f.edits();
        let nl = &edits.new;
        for op in &edits.ops {
            let (start, len) = match *op {
                DiffOp::Insert { new_index, new_len, .. } => (new_index, new_len),
                DiffOp::Replace { new_index, new_len, .. } => (new_index, new_len),
                _ => continue,
            };
            for i in start..start + len {
                let raw = String::from_utf8_lossy(nl[i]);
                let line = raw.trim_end_matches('\n').trim_end_matches('\r');
                let mut problems = Vec::new();
                if line.ends_with(' ') || line.ends_with('\t') {
                    problems.push("trailing whitespace");
                }
                let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                if indent.contains(" \t") {
                    problems.push("space before tab in indent");
                }
                if line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ") || line == "=======" {
                    problems.push("leftover conflict marker");
                }
                if !problems.is_empty() {
                    s.push_str(&format!("{}:{}: {}.\n+{}\n", f.path(), i + 1, problems.join(", "), line));
                }
            }
        }
    }
    s
}

#[cfg(test)]
pub(crate) mod regression {
    use super::*;
    pub(crate) fn generated_files() -> Vec<FileDiff> {
        let mut files = Vec::new();
        for n in 0..12 {
            let old = (0..n).map(|i| format!("line {i}\n")).collect::<String>().into_bytes();
            let mut new = old.clone();
            new.extend_from_slice(if n % 2 == 0 { b"added\n" } else { b"tail" });
            let side = |path: String, data| Side { path, data, mode: 0o100644, big: None };
            files.push(FileDiff {
                old: (n % 3 != 0).then(|| side(format!("old-{n}"), old)),
                new: (n % 3 != 1).then(|| side(format!("new-{n}"), new)),
            });
        }
        for data in [b"binary\0data".as_slice(), b"", b"unchanged\n", b"a\nb\n"] {
            let side = |path: &str| Side { path: path.into(), data: data.into(), mode: 0o100644, big: None };
            files.push(FileDiff { old: Some(side("before")), new: Some(side("after")) });
        }
        for (old, new, mode, big) in [
            (b"old\0binary".as_slice(), b"new\0binary".as_slice(), 0o100644, None),
            (b"before\r\n", b"after \r\n \tindent\n<<<<<<< conflict\n", 0o100755, None),
            (b"valid\n", b"invalid\xff", 0o120000, None),
            (b"", b"", 0o100644, Some(layr_model::diff::MAX_TEXT + 1)),
        ] {
            files.push(FileDiff {
                old: Some(Side { path: "old path".into(), data: old.into(), mode: 0o100644, big }),
                new: Some(Side { path: "new path".into(), data: new.into(), mode, big }),
            });
        }
        files
    }

    #[test]
    fn generated_combined_renderers_match_previous_bytes() {
        let files = generated_files();
        // Captured from the previous formatters on the generated fixture below.
        let expected: serde_json::Value = serde_json::from_str(include_str!("diff-regression.json")).unwrap();
        let analyzed: Vec<_> = files.iter().map(AnalyzedDiff::new).collect();
        let actual = serde_json::json!([
            stat_analyzed(&analyzed, 80),
            numstat_analyzed(&analyzed),
            shortstat_analyzed(&analyzed),
            name_only_analyzed(&analyzed),
            name_status_analyzed(&analyzed),
            check_analyzed(&analyzed),
            analyzed.iter().flat_map(|f| (0..5).map(move |c| patch_analyzed(f, c))).collect::<Vec<_>>()
        ]);
        assert_eq!(actual, expected);
        for f in &analyzed {
            if !f.is_binary() {
                let edits = f.edits() as *const _;
                f.counts();
                patch_analyzed(f, 3);
                assert!(std::ptr::eq(edits, f.edits()));
            }
            if let Some(side) = &f.new {
                let source = files.iter().filter_map(|f| f.new.as_ref()).find(|s| std::ptr::eq(*s, side)).unwrap();
                assert!(std::ptr::eq(source.data.as_ptr(), side.data.as_ptr()));
            }
        }
    }
}

#[cfg(test)]
mod measurements {
    use super::*;

    fn generated_file() -> FileDiff {
        let old = (0..20_000).flat_map(|i| format!("generated line {i}\n").into_bytes()).collect::<Vec<_>>();
        let mut new = old.clone();
        new.extend_from_slice(b"one added line\n");
        FileDiff {
            old: Some(Side { path: "before".into(), mode: 0o100644, data: old, big: None }),
            new: Some(Side { path: "after".into(), mode: 0o100644, data: new, big: None }),
        }
    }

    #[test]
    #[ignore = "generated allocation measurement"]
    fn measure_renderer_content_allocations() {
        let files = vec![generated_file()];
        if std::env::var_os("LAYR_PERF_BASELINE").is_some() {
            let owned = files.clone();
            std::hint::black_box(owned.iter().map(|f| f.path().to_string()).collect::<Vec<_>>());
        } else {
            let analyzed: Vec<_> = files.iter().map(AnalyzedDiff::new).collect();
            std::hint::black_box(name_only_analyzed(&analyzed));
        }
    }

    #[test]
    #[ignore = "generated allocation measurement"]
    fn measure_shared_edit_allocations() {
        let files = vec![generated_file()];
        if std::env::var_os("LAYR_PERF_BASELINE").is_some() {
            std::hint::black_box(stat(&files, 80));
            std::hint::black_box(
                files
                    .iter()
                    .map(|f| {
                        let (a, d) = f.counts();
                        format!("{a}\t{d}\t{}\n", f.display_path())
                    })
                    .collect::<String>(),
            );
            std::hint::black_box(shortstat(&files));
            // Before sharing, rename similarity and patch hunks each captured operations.
            std::hint::black_box(files.iter().map(FileDiff::similarity).collect::<Vec<_>>());
            std::hint::black_box(files.iter().map(|f| patch(f, 3)).collect::<Vec<_>>());
        } else {
            let analyzed: Vec<_> = files.iter().map(AnalyzedDiff::new).collect();
            std::hint::black_box(stat_analyzed(&analyzed, 80));
            std::hint::black_box(numstat_analyzed(&analyzed));
            std::hint::black_box(shortstat_analyzed(&analyzed));
            std::hint::black_box(analyzed.iter().map(|f| patch_analyzed(f, 3)).collect::<Vec<_>>());
        }
    }
}
