// Prototype: agent tool substitution. A daemon keeps the tree index in memory and answers
// tree-walking tools (rg -c, rg --files, find -type f, status) over a Unix socket.
// File contents are read from the backing paths, as blobs on the SSD would be.
//
//   tooldaemon serve <root> <socket>
//   tooldaemon client <socket> <rgc PATTERN | files | find | status>

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;

struct Index {
    root: PathBuf,
    all: Vec<String>,     // every regular file, like find -type f
    visible: Vec<String>, // files rg would search (gitignore, hidden rules)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

fn build(root: &Path) -> Index {
    let t = Instant::now();
    let mut all = Vec::new();
    for e in walkdir(root) {
        all.push(e);
    }
    let mut visible = Vec::new();
    for e in ignore::WalkBuilder::new(root).build().flatten() {
        if e.file_type().map_or(false, |t| t.is_file()) {
            visible.push(rel(root, e.path()));
        }
    }
    eprintln!("index: {} files, {} visible, built in {:?}", all.len(), visible.len(), t.elapsed());
    Index { root: root.to_path_buf(), all, visible }
}

fn walkdir(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                out.push(rel(root, &e.path()));
            }
        }
    }
    out
}

struct Count(u64);
impl Sink for Count {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.0 += 1;
        Ok(true)
    }
}

fn rgc(ix: &Index, pattern: &str, out: &mut impl Write) -> std::io::Result<()> {
    let m = RegexMatcher::new_line_matcher(pattern).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hits: Vec<(&String, u64)> = ix
        .visible
        .par_iter()
        .map_init(
            || SearcherBuilder::new().binary_detection(BinaryDetection::quit(0)).build(),
            |s, p| {
                let mut c = Count(0);
                let _ = s.search_path(&m, ix.root.join(p), &mut c);
                (p, c.0)
            },
        )
        .filter(|(_, n)| *n > 0)
        .collect();
    for (p, n) in hits {
        writeln!(out, "{p}:{n}")?;
    }
    Ok(())
}

fn serve(root: &str, sock: &str) {
    let ix = Arc::new(build(Path::new(root)));
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock).expect("bind");
    eprintln!("listening on {sock}");
    for s in l.incoming().flatten() {
        let ix = ix.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(&s);
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                return;
            }
            let mut w = std::io::BufWriter::with_capacity(1 << 20, &s);
            let line = line.trim_end_matches('\n');
            let (cmd, arg) = line.split_once('\t').unwrap_or((line, ""));
            let _ = match cmd {
                "rgc" => rgc(&ix, arg, &mut w),
                "files" => ix.visible.iter().try_for_each(|p| writeln!(w, "{p}")),
                "find" => ix.all.iter().try_for_each(|p| writeln!(w, "./{p}")),
                "status" => Ok(()), // the change layer is empty in this test
                _ => writeln!(w, "unknown command"),
            };
            let _ = w.flush();
        });
    }
}

fn client(sock: &str, cmd: &str, arg: Option<&str>) {
    let mut s = UnixStream::connect(sock).expect("connect");
    let req = match arg {
        Some(a) => format!("{cmd}\t{a}\n"),
        None => format!("{cmd}\n"),
    };
    s.write_all(req.as_bytes()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    std::io::stdout().write_all(&buf).unwrap();
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("serve") => serve(&a[2], &a[3]),
        Some("client") => client(&a[2], &a[3], a.get(4).map(String::as_str)),
        _ => eprintln!("usage: tooldaemon serve <root> <sock> | client <sock> <cmd> [arg]"),
    }
}
