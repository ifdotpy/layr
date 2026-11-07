//! `find` served from the index, for the expressions agents use most: start paths, `-type`,
//! `-name`, `-iname`, `-path`, `-ipath`, `-not`/`!`, `-maxdepth`, `-mindepth` and `-print`, joined
//! by an implicit or explicit `-a`. Entries are printed in sorted pre-order (the real `find` prints
//! in directory order). Anything else returns `Fallback`.

use super::index::{self, Kind, Tree};
use super::Outcome;
use globset::{GlobBuilder, GlobMatcher};
use std::io::Write;
use std::path::{Path, PathBuf};

enum Test {
    Type(Kind),
    Name(GlobMatcher),
    Path(GlobMatcher),
}

struct Cond {
    negate: bool,
    test: Test,
}

struct Expr {
    starts: Vec<String>,
    conds: Vec<Cond>,
    max_depth: Option<usize>,
    min_depth: usize,
}

fn glob(p: &str, case_insensitive: bool) -> Result<GlobMatcher, String> {
    if p.contains('{') || p.contains('}') {
        return Err("braces differ between fnmatch and globs".into());
    }
    // `*` matches `/` in `-path`, as fnmatch without FNM_PATHNAME does.
    let g = GlobBuilder::new(p)
        .literal_separator(false)
        .case_insensitive(case_insensitive)
        .backslash_escape(true)
        .build()
        .map_err(|e| format!("pattern {p}: {e}"))?;
    Ok(g.compile_matcher())
}

fn parse(args: &[String]) -> Result<Expr, String> {
    let mut i = 0;
    let mut starts = Vec::new();
    while i < args.len() && !args[i].starts_with('-') && args[i] != "!" && args[i] != "(" {
        starts.push(args[i].clone());
        i += 1;
    }
    if starts.is_empty() {
        return Err("no start path".into());
    }
    let mut e = Expr { starts, conds: Vec::new(), max_depth: None, min_depth: 0 };
    let mut negate = false;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        let value = |i: &mut usize| -> Result<String, String> {
            let v = args.get(*i).cloned().ok_or_else(|| format!("{a} needs a value"))?;
            *i += 1;
            Ok(v)
        };
        let test = match a {
            "!" | "-not" => {
                negate = !negate;
                continue;
            }
            "-a" | "-and" | "-print" => continue,
            "-maxdepth" => {
                e.max_depth = Some(value(&mut i)?.parse().map_err(|_| "-maxdepth takes a number")?);
                continue;
            }
            "-mindepth" => {
                e.min_depth = value(&mut i)?.parse().map_err(|_| "-mindepth takes a number")?;
                continue;
            }
            "-type" => match value(&mut i)?.as_str() {
                "f" => Test::Type(Kind::File),
                "d" => Test::Type(Kind::Dir),
                "l" => Test::Type(Kind::Link),
                t => return Err(format!("-type {t}")),
            },
            "-name" => Test::Name(glob(&value(&mut i)?, false)?),
            "-iname" => Test::Name(glob(&value(&mut i)?, true)?),
            "-path" | "-wholename" => Test::Path(glob(&value(&mut i)?, false)?),
            "-ipath" | "-iwholename" => Test::Path(glob(&value(&mut i)?, true)?),
            other => return Err(format!("unsupported expression {other}")),
        };
        e.conds.push(Cond { negate, test });
        negate = false;
    }
    if negate {
        return Err("a trailing -not".into());
    }
    if e.max_depth.is_some_and(|m| m < e.min_depth) {
        return Err("-maxdepth below -mindepth".into());
    }
    Ok(e)
}

impl Expr {
    fn matches(&self, shown: &str, kind: Kind) -> bool {
        let name = shown.rsplit('/').next().unwrap_or(shown);
        self.conds.iter().all(|c| {
            let r = match &c.test {
                Test::Type(k) => *k == kind,
                Test::Name(g) => g.is_match(name),
                Test::Path(g) => g.is_match(shown),
            };
            r != c.negate
        })
    }
}

fn resolve(cwd: &Path, arg: &str) -> Option<PathBuf> {
    if arg.is_empty() || arg.starts_with('/') || arg.ends_with('/') {
        return None; // trailing slashes print differently in BSD and GNU find
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

pub fn run(cwd: &Path, args: &[String], tree: &Tree, out: &mut dyn Write) -> Outcome {
    let e = match parse(args) {
        Ok(e) => e,
        Err(why) => return Outcome::Fallback(why),
    };
    // Resolve every start first: an unknown start falls back before anything is printed.
    let mut starts = Vec::new();
    for s in &e.starts {
        let Some(rel) = resolve(cwd, s) else { return Outcome::Fallback(format!("start {s} leaves the tree")) };
        let k = index::key(&rel);
        let kind = if k.is_empty() { Some(Kind::Dir) } else { tree.all.get(&k).copied() };
        match kind {
            Some(kind) => starts.push((s, k, kind)),
            None => return Outcome::Fallback(format!("start {s} does not exist")),
        }
    }
    for (s, k, kind) in starts {
        if e.min_depth == 0 && e.matches(s, kind) {
            let _ = writeln!(out, "{s}");
        }
        if kind != Kind::Dir || e.max_depth == Some(0) {
            continue;
        }
        let skip = if k.is_empty() { 0 } else { k.len() + 1 };
        for (key, kind) in index::below(&tree.all, &k) {
            let below = &key[skip..];
            let depth = 1 + below.iter().filter(|&&b| b == index::SEP).count();
            if depth < e.min_depth || e.max_depth.is_some_and(|m| depth > m) {
                continue;
            }
            let shown = format!("{}/{}", s, index::rel_path(below).display());
            if e.matches(&shown, *kind) {
                let _ = writeln!(out, "{shown}");
            }
        }
    }
    Outcome::Done(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_common_expressions() {
        let e = parse(&args(". -type f -name *.rs -not -path ./target/*")).unwrap();
        assert_eq!(e.conds.len(), 3);
        assert!(e.matches("./src/a.rs", Kind::File));
        assert!(!e.matches("./target/a.rs", Kind::File));
        assert!(!e.matches("./src", Kind::Dir));
        assert!(parse(&args(". -newer x")).is_err());
        assert!(parse(&args(". -o -name x")).is_err());
        assert!(parse(&args("-name x")).is_err());
        assert!(parse(&args(". -name {a,b}")).is_err());
    }
}
