//! `rg` served from the index: the ripgrep engine (`grep-searcher`, `grep-regex`, `grep-printer`)
//! over the files that the index lists, with the output of `rg --sort path` when stdout is not a
//! terminal. A flag or a path outside the supported set returns `Fallback`, and the client runs
//! the real `rg`.

use super::index::{self, Kind, Tree};
use super::Outcome;
use grep_printer::{StandardBuilder, SummaryBuilder, SummaryKind};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Lines,
    Count,
    CountMatches,
    FilesWith,
    FilesWithout,
}

struct Opts {
    patterns: Vec<String>,
    paths: Vec<String>,
    files: bool,
    case: Case,
    fixed: bool,
    word: bool,
    line: bool,
    invert: bool,
    mode: Mode,
    quiet: bool,
    line_number: bool,
    with_filename: Option<bool>,
    only_matching: bool,
    after: usize,
    before: usize,
    max_count: Option<u64>,
    neg_globs: Vec<String>,
    types: Vec<String>,
    type_nots: Vec<String>,
    no_messages: bool,
}

fn fallback<T>(why: impl Into<String>) -> Result<T, String> {
    Err(why.into())
}

fn number(v: &str, flag: &str) -> Result<usize, String> {
    v.parse().map_err(|_| format!("{flag} takes a number"))
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        patterns: Vec::new(),
        paths: Vec::new(),
        files: false,
        case: Case::Sensitive,
        fixed: false,
        word: false,
        line: false,
        invert: false,
        mode: Mode::Lines,
        quiet: false,
        line_number: false,
        with_filename: None,
        only_matching: false,
        after: 0,
        before: 0,
        max_count: None,
        neg_globs: Vec::new(),
        types: Vec::new(),
        type_nots: Vec::new(),
        no_messages: false,
    };
    let mut positional = Vec::new();
    let mut i = 0;
    let mut context: Option<usize> = None;
    let (mut after, mut before) = (None, None);
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            positional.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let value = |i: &mut usize| -> Result<String, String> {
                if let Some(v) = inline.clone() {
                    return Ok(v);
                }
                let v = args.get(*i).cloned().ok_or_else(|| format!("--{name} needs a value"))?;
                *i += 1;
                Ok(v)
            };
            match name {
                "ignore-case" => o.case = Case::Insensitive,
                "case-sensitive" => o.case = Case::Sensitive,
                "smart-case" => o.case = Case::Smart,
                "fixed-strings" => o.fixed = true,
                "word-regexp" => o.word = true,
                "line-regexp" => o.line = true,
                "invert-match" => o.invert = true,
                "count" => o.mode = Mode::Count,
                "count-matches" => o.mode = Mode::CountMatches,
                "files-with-matches" => o.mode = Mode::FilesWith,
                "files-without-match" => o.mode = Mode::FilesWithout,
                "line-number" => o.line_number = true,
                "no-line-number" => o.line_number = false,
                "with-filename" => o.with_filename = Some(true),
                "no-filename" => o.with_filename = Some(false),
                "only-matching" => o.only_matching = true,
                "after-context" => after = Some(number(&value(&mut i)?, "--after-context")?),
                "before-context" => before = Some(number(&value(&mut i)?, "--before-context")?),
                "context" => context = Some(number(&value(&mut i)?, "--context")?),
                "max-count" => o.max_count = Some(number(&value(&mut i)?, "--max-count")? as u64),
                "regexp" => o.patterns.push(value(&mut i)?),
                "glob" => {
                    let g = value(&mut i)?;
                    if !g.starts_with('!') {
                        return fallback("a whitelist --glob overrides ignore rules");
                    }
                    o.neg_globs.push(g);
                }
                "type" => o.types.push(value(&mut i)?),
                "type-not" => o.type_nots.push(value(&mut i)?),
                "files" => o.files = true,
                "quiet" => o.quiet = true,
                "no-messages" => o.no_messages = true,
                "no-heading" | "no-config" => {}
                "threads" => {
                    value(&mut i)?;
                }
                "color" | "colour" => {
                    let v = value(&mut i)?;
                    if v != "never" && v != "auto" {
                        return fallback("--color other than never or auto");
                    }
                }
                "sort" => {
                    let v = value(&mut i)?;
                    if v != "path" && v != "none" {
                        return fallback("--sort other than path");
                    }
                }
                _ => return fallback(format!("unsupported flag --{name}")),
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let c = chars[k];
                k += 1;
                // Flags that take a value: the rest of the cluster, or the next argument.
                let mut take = |i: &mut usize| -> Result<String, String> {
                    let rest: String = chars[k..].iter().collect();
                    k = chars.len();
                    if !rest.is_empty() {
                        return Ok(rest);
                    }
                    let v = args.get(*i).cloned().ok_or_else(|| format!("-{c} needs a value"))?;
                    *i += 1;
                    Ok(v)
                };
                match c {
                    'i' => o.case = Case::Insensitive,
                    's' => o.case = Case::Sensitive,
                    'S' => o.case = Case::Smart,
                    'F' => o.fixed = true,
                    'w' => o.word = true,
                    'x' => o.line = true,
                    'v' => o.invert = true,
                    'c' => o.mode = Mode::Count,
                    'l' => o.mode = Mode::FilesWith,
                    'n' => o.line_number = true,
                    'N' => o.line_number = false,
                    'H' => o.with_filename = Some(true),
                    'I' => o.with_filename = Some(false),
                    'o' => o.only_matching = true,
                    'q' => o.quiet = true,
                    'A' => after = Some(number(&take(&mut i)?, "-A")?),
                    'B' => before = Some(number(&take(&mut i)?, "-B")?),
                    'C' => context = Some(number(&take(&mut i)?, "-C")?),
                    'm' => o.max_count = Some(number(&take(&mut i)?, "-m")? as u64),
                    'e' => o.patterns.push(take(&mut i)?),
                    'g' => {
                        let g = take(&mut i)?;
                        if !g.starts_with('!') {
                            return fallback("a whitelist -g overrides ignore rules");
                        }
                        o.neg_globs.push(g);
                    }
                    't' => o.types.push(take(&mut i)?),
                    'T' => o.type_nots.push(take(&mut i)?),
                    'j' => {
                        take(&mut i)?;
                    }
                    _ => return fallback(format!("unsupported flag -{c}")),
                }
            }
            continue;
        }
        positional.push(a.clone());
    }
    o.after = after.or(context).unwrap_or(0);
    o.before = before.or(context).unwrap_or(0);
    let mut positional = positional.into_iter();
    if o.patterns.is_empty() && !o.files {
        match positional.next() {
            Some(p) => o.patterns.push(p),
            None => return fallback("no pattern"),
        }
    }
    o.paths = positional.collect();
    Ok(o)
}

/// A searched root as given on the command line.
enum Target {
    /// A folder walked with the ignore rules. `arg` is `None` for the implicit working folder.
    Dir { key: Vec<u8>, arg: Option<String> },
    /// An explicit file, searched even when ignored.
    File { key: Vec<u8>, arg: String },
}

/// Root-relative components of `cwd/arg`, or `None` when `..` or an absolute path appears.
fn resolve(cwd: &Path, arg: &str) -> Option<PathBuf> {
    if arg.is_empty() || arg.starts_with('/') {
        return None;
    }
    let mut out = cwd.to_path_buf();
    for comp in arg.split('/') {
        match comp {
            "" | "." => {}
            ".." => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

pub fn run(
    root: &Path,
    cwd: &Path,
    stdin_readable: bool,
    args: &[String],
    tree: &Tree,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Outcome {
    match run_inner(root, cwd, stdin_readable, args, tree, out, err) {
        Ok(code) => Outcome::Done(code),
        Err(why) => Outcome::Fallback(why),
    }
}

fn run_inner(
    root: &Path,
    cwd: &Path,
    stdin_readable: bool,
    args: &[String],
    tree: &Tree,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let o = parse(args)?;
    if o.paths.is_empty() && stdin_readable && !o.files {
        return fallback("rg reads standard input");
    }
    let cwd_key = index::key(cwd);
    if !cwd_key.is_empty() && !tree.walked.contains(&cwd_key) {
        return fallback("the working folder is not walked by rg (ignored or hidden)");
    }
    let mut targets = Vec::new();
    if o.paths.is_empty() {
        targets.push(Target::Dir { key: cwd_key.clone(), arg: None });
    }
    for p in &o.paths {
        let rel = resolve(cwd, p).ok_or_else(|| format!("path {p} leaves the tree"))?;
        let k = index::key(&rel);
        if k.is_empty() {
            targets.push(Target::Dir { key: k, arg: Some(p.clone()) });
            continue;
        }
        match tree.all.get(&k) {
            Some(Kind::Dir) if tree.walked.contains(&k) => targets.push(Target::Dir { key: k, arg: Some(p.clone()) }),
            Some(Kind::File) => targets.push(Target::File { key: k, arg: p.clone() }),
            _ => return fallback(format!("path {p} is missing, ignored, hidden or a link")),
        }
    }
    let with_filename = o.with_filename.unwrap_or(!(targets.len() == 1 && matches!(targets[0], Target::File { .. })));
    let filters = Filters::new(&o, root, cwd)?;

    // The files in output order, with the path that is printed.
    let mut files: Vec<(PathBuf, String, bool)> = Vec::new(); // (absolute, shown, explicit)
    for t in &targets {
        match t {
            Target::File { key, arg } => files.push((root.join(index::rel_path(key)), arg.clone(), true)),
            Target::Dir { key, arg } => {
                for f in index::below_set(&tree.visible, key) {
                    let rel = index::rel_path(f);
                    let below = index::rel_path(&f[if key.is_empty() { 0 } else { key.len() + 1 }..]);
                    if !filters.keep(&rel, cwd) {
                        continue;
                    }
                    let shown = match arg {
                        None => below.display().to_string(),
                        Some(a) => Path::new(a).join(&below).display().to_string(),
                    };
                    files.push((root.join(&rel), shown, false));
                }
            }
        }
    }

    if o.files {
        let mut listed = false;
        for (_, shown, _) in &files {
            listed = true;
            if !o.quiet {
                let _ = writeln!(out, "{shown}");
            }
        }
        return Ok(if listed { 0 } else { 1 });
    }

    let matcher = build_matcher(&o)?;
    let results: Vec<(Vec<u8>, bool, Option<String>)> =
        files.par_iter().map(|(abs, shown, explicit)| search_one(&o, &matcher, abs, shown, *explicit, with_filename)).collect();
    let (mut matched, mut failed, mut printed) = (false, false, false);
    let separate = o.mode == Mode::Lines && (o.after > 0 || o.before > 0);
    for (bytes, m, e) in results {
        matched |= m;
        if let Some(e) = e {
            failed = true;
            if !o.no_messages {
                let _ = writeln!(err, "rg: {e}");
            }
        }
        if o.quiet || bytes.is_empty() {
            continue;
        }
        if separate && printed {
            let _ = out.write_all(b"--\n");
        }
        let _ = out.write_all(&bytes);
        printed = true;
    }
    Ok(if failed && !(o.quiet && matched) {
        2
    } else if matched {
        0
    } else {
        1
    })
}

fn build_matcher(o: &Opts) -> Result<RegexMatcher, String> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(o.case == Case::Insensitive)
        .case_smart(o.case == Case::Smart)
        .fixed_strings(o.fixed)
        .word(o.word)
        .whole_line(o.line)
        .line_terminator(Some(b'\n'));
    b.build_many(&o.patterns).map_err(|e| format!("the pattern is rejected here ({e}); rg reports it"))
}

fn search_one(
    o: &Opts,
    matcher: &RegexMatcher,
    abs: &Path,
    shown: &str,
    explicit: bool,
    with_filename: bool,
) -> (Vec<u8>, bool, Option<String>) {
    let mut searcher = SearcherBuilder::new()
        .line_number(o.line_number)
        .invert_match(o.invert)
        .after_context(if o.mode == Mode::Lines { o.after } else { 0 })
        .before_context(if o.mode == Mode::Lines { o.before } else { 0 })
        .max_matches(o.max_count)
        .binary_detection(if explicit { BinaryDetection::convert(0) } else { BinaryDetection::quit(0) })
        .build();
    let shown_path = Path::new(shown);
    let error = |e: std::io::Error| Some(format!("{shown}: {e}"));
    match o.mode {
        Mode::Lines => {
            let mut printer =
                StandardBuilder::new().path(with_filename).only_matching(o.only_matching).build_no_color(Vec::new());
            let mut sink = printer.sink_with_path(matcher, shown_path);
            let r = searcher.search_path(matcher, abs, &mut sink);
            let m = sink.has_match();
            drop(sink);
            (printer.into_inner().into_inner(), m, r.err().and_then(error))
        }
        _ => {
            let kind = match o.mode {
                Mode::Count => SummaryKind::Count,
                Mode::CountMatches => SummaryKind::CountMatches,
                Mode::FilesWith => SummaryKind::PathWithMatch,
                _ => SummaryKind::PathWithoutMatch,
            };
            let mut printer = SummaryBuilder::new().kind(kind).path(with_filename).build_no_color(Vec::new());
            let mut sink = printer.sink_with_path(matcher, shown_path);
            let r = searcher.search_path(matcher, abs, &mut sink);
            let m = sink.has_match();
            drop(sink);
            let matched = if o.mode == Mode::FilesWithout { !m } else { m };
            (printer.into_inner().into_inner(), matched, r.err().and_then(error))
        }
    }
}

/// `-g '!glob'`, `-t` and `-T`, applied to walked files (explicit files are always searched).
struct Filters {
    globs: Option<ignore::overrides::Override>,
    types: Option<ignore::types::Types>,
}

impl Filters {
    fn new(o: &Opts, root: &Path, cwd: &Path) -> Result<Filters, String> {
        let globs = if o.neg_globs.is_empty() {
            None
        } else {
            let mut b = ignore::overrides::OverrideBuilder::new(root.join(cwd));
            for g in &o.neg_globs {
                b.add(g).map_err(|e| format!("glob {g}: {e}"))?;
            }
            Some(b.build().map_err(|e| e.to_string())?)
        };
        let types = if o.types.is_empty() && o.type_nots.is_empty() {
            None
        } else {
            let mut b = ignore::types::TypesBuilder::new();
            b.add_defaults();
            for t in &o.types {
                b.select(t);
            }
            for t in &o.type_nots {
                b.negate(t);
            }
            Some(b.build().map_err(|e| format!("file type: {e}"))?)
        };
        Ok(Filters { globs, types })
    }

    /// `rel` is root-relative; globs match relative to the working folder.
    fn keep(&self, rel: &Path, cwd: &Path) -> bool {
        if let Some(t) = &self.types {
            if t.matched(rel, false).is_ignore() {
                return false;
            }
        }
        if let Some(g) = &self.globs {
            let Ok(local) = rel.strip_prefix(cwd) else { return true };
            if g.matched(local, false).is_ignore() {
                return false;
            }
            let mut parent = local.parent();
            while let Some(p) = parent {
                if p.as_os_str().is_empty() {
                    break;
                }
                if g.matched(p, true).is_ignore() {
                    return false;
                }
                parent = p.parent();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_clusters_and_values() {
        let o = parse(&args("-inC2 -e foo -e bar src -g !*.md")).unwrap();
        assert!(o.case == Case::Insensitive && o.line_number);
        assert_eq!((o.after, o.before), (2, 2));
        assert_eq!(o.patterns, ["foo", "bar"]);
        assert_eq!(o.paths, ["src"]);
        assert_eq!(o.neg_globs, ["!*.md"]);
        let o = parse(&args("-A1 --max-count=3 pat")).unwrap();
        assert_eq!((o.after, o.before, o.max_count), (1, 0, Some(3)));
        assert_eq!(o.patterns, ["pat"]);
    }

    #[test]
    fn unsupported_flags_fall_back() {
        assert!(parse(&args("--hidden foo")).is_err());
        assert!(parse(&args("-uu foo")).is_err());
        assert!(parse(&args("-g *.rs foo")).is_err());
        assert!(parse(&args("--json foo")).is_err());
        assert!(parse(&args("")).is_err());
    }

    #[test]
    fn resolve_rejects_leaving_paths() {
        assert_eq!(resolve(Path::new("a"), "./b/"), Some(PathBuf::from("a/b")));
        assert_eq!(resolve(Path::new(""), "."), Some(PathBuf::new()));
        assert_eq!(resolve(Path::new("a"), "../b"), None);
        assert_eq!(resolve(Path::new("a"), "/etc"), None);
    }
}
