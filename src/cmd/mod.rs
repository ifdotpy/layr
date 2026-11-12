//! Command dispatch and helpers shared by commands.

pub mod admin;
pub mod attest;
pub mod bisect;
pub mod branch;
pub mod change;
pub mod checks;
pub mod du;
pub mod help;
pub mod followcmd;
pub mod history;
pub mod layerpush;
pub mod mergecmd;
pub mod migrate;
pub mod objects;
pub mod oci;
pub mod overlaycmd;
pub mod park;
pub mod probe;
pub mod project;
pub mod queue;
pub mod refs;
pub mod release;
pub mod release_prepare;
pub mod remote;
pub mod replicate;
pub mod requests;
pub mod review;
pub mod transfer;
pub mod view;
pub mod volume;
pub mod work;

use crate::ctx::{Ctx, Loc};
use crate::model::Line;
use crate::repo::Repo;
use anyhow::{bail, Result};

/// An error with a specific exit code (git uses 1 for "not done", 128 for fatal errors).
#[derive(Debug)]
pub struct Exit(pub i32, pub String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}
impl std::error::Error for Exit {}

pub fn exit(code: i32, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Exit(code, msg.into()))
}

/// The names of lines for JSON output, next to their ids (docs/commands.md, "Line names in JSON").
/// Each line is looked up once.
pub(crate) struct LineNames<'d> {
    db: &'d crate::db::Db,
    seen: std::collections::BTreeMap<String, Option<String>>,
}

impl<'d> LineNames<'d> {
    pub(crate) fn new(db: &'d crate::db::Db) -> Self {
        Self { db, seen: Default::default() }
    }

    /// The name of line `id`, also of a deleted line; null when the project knows no name.
    pub(crate) fn name(&mut self, id: &str) -> Result<serde_json::Value> {
        if !self.seen.contains_key(id) {
            let name = self.db.line_name(id)?;
            self.seen.insert(id.to_string(), name);
        }
        Ok(serde_json::json!(self.seen[id]))
    }

    /// Add `<key>_name` next to the line id `key` of `value` (an object) when it has that key. A key
    /// that is no line id (null) gets a null name.
    pub(crate) fn add(&mut self, value: &mut serde_json::Value, key: &str) -> Result<()> {
        let Some(id) = value.get(key).map(|v| v.as_str().map(str::to_owned)) else {
            return Ok(());
        };
        value[format!("{key}_name")] = match id {
            Some(id) => self.name(&id)?,
            None => serde_json::Value::Null,
        };
        Ok(())
    }
}

/// One line on stderr for a cache flag that goes (docs/overlays.md, section 1a, slice 6): a cache
/// is an ordinary overlay. The flag keeps working for one release, so runners have time to change.
/// The overlay flags of a command (owner decision 2026-11-11: a cache is an ordinary overlay, so no flag names caches).
/// Each old spelling in `renames` (old, new) fails with exit status 2 and names the new one; the new spelling maps to
/// the old one, which the command's code still reads.
pub fn overlay_args(op: &str, args: &[String], renames: &[(&str, &str)]) -> Result<Vec<String>> {
    for (old, new) in renames {
        if args.iter().any(|a| a == old || a.starts_with(&format!("{old}="))) {
            return Err(exit(2, format!("layr {op} {old} was removed: a cache is an ordinary overlay. Use layr {op} {new}")));
        }
    }
    Ok(args
        .iter()
        .map(|a| renames.iter().find(|(_, new)| a == new).map(|(old, _)| old.to_string()).unwrap_or_else(|| a.clone()))
        .collect())
}


pub struct Here<'a> {
    pub repo: Repo<'a>,
    pub line: Option<Line>,
    pub loc: Loc,
}

impl<'a> Here<'a> {
    pub fn line(&self) -> Result<&Line> {
        match &self.line {
            Some(l) => Ok(l),
            None => bail!("not in a line: cd into lines/<name> of project {}, or add --line <name>", self.repo.p.name),
        }
    }

    /// Convert command line paths (relative to the working folder) to paths from the line root.
    pub fn specs(&self, args: &[String]) -> Result<Vec<String>> {
        // Many paths in one argument (zsh does not split an unquoted `$var`): name the cause, not ENAMETOOLONG.
        if let Some(arg) = args.iter().find(|a| a.trim_end_matches('\n').contains('\n')) {
            let lines = arg.lines().filter(|l| !l.is_empty()).count();
            return Err(exit(
                128,
                format!(
                    "one argument holds {lines} lines; pass each path as its own argument (in zsh: ${{=var}} or ${{(f)var}})"
                ),
            ));
        }
        args.iter().map(|a| normalize(&self.loc.sub, a)).collect()
    }

    /// Show a root-relative path relative to the working folder, as git does.
    pub fn rel(&self, p: &str) -> String {
        relative_to(&self.loc.sub, p)
    }
}

thread_local! {
    /// A head without a line for the reading commands of `layr overlay log|show|diff|grep|ls-files`
    /// on an overlay that keeps parts (docs/overlays.md, section 1a): the project and the line, whose
    /// head is the part that the current line shows. Only `overlaycmd::read` sets it.
    static DETACHED: std::cell::RefCell<Option<(String, Line)>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with `line` as the line of the commands in `project` that name no line.
pub(crate) fn with_detached<T>(project: &str, line: Line, f: impl FnOnce() -> T) -> T {
    DETACHED.with(|d| *d.borrow_mut() = Some((project.to_string(), line)));
    let out = f();
    DETACHED.with(|d| *d.borrow_mut() = None);
    out
}

pub fn here(ctx: &Ctx) -> Result<Here<'_>> {
    let loc = ctx.locate()?;
    crate::store::set_holder_line(loc.line.as_deref());
    let repo = Repo::open(ctx, &loc.project)?;
    let line = match &loc.line {
        Some(n) => Some(repo.line(n)?),
        None => DETACHED.with(|d| d.borrow().as_ref().filter(|(p, _)| *p == loc.project).map(|(_, l)| l.clone())),
    };
    // The local ignore file of the line applies also where a command scans a copy of the folder.
    if let Some(l) = line.as_ref().filter(|_| loc.line.is_some()) {
        crate::ignore_rules::set_line_exclude(Some(repo.working(l).join(crate::ignore_rules::LOCAL_EXCLUDE)));
    }
    Ok(Here { repo, line, loc })
}

pub fn normalize(sub: &str, arg: &str) -> Result<String> {
    if arg.starts_with('/') {
        bail!("absolute paths are not supported: {arg}");
    }
    let mut parts: Vec<String> = if sub.is_empty() { vec![] } else { sub.split('/').map(|s| s.to_string()).collect() };
    for c in arg.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("path outside the line: {arg}");
                }
            }
            x => parts.push(x.to_string()),
        }
    }
    Ok(parts.join("/"))
}

pub fn relative_to(sub: &str, p: &str) -> String {
    if sub.is_empty() {
        return p.to_string();
    }
    let s: Vec<&str> = sub.split('/').collect();
    let t: Vec<&str> = p.split('/').collect();
    let mut i = 0;
    while i < s.len() && i < t.len() && s[i] == t[i] {
        i += 1;
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", s.len() - i).collect();
    out.extend_from_slice(&t[i..]);
    out.join("/")
}

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

pub const HELP: &str = "usage: layr <command> [<args>]\n   store-version [--json]
   schema [<name>] (JSON Schemas of --json output), store remove <path> [--force]

Work in a line:
   status [--json] [--no-overlaps] [--no-ignored]
   diff, add, rm, mv, restore, reset, save -m <message>, park, clean, apply
Read history:
   log, show, blame, grep, ls-files, rev-parse, merge-base
Lines and states:
   line new <name> [<rev>] [--count N], line list, line delete, line rename, line move <line> [<path>]
   save -m <message> [-a] [--amend]
   save [--note <text>] [--all-lines] [--running] [--force] [--caches [--cache <path>]] [--keep <name>]
   keep [list [--json] | drop <name>... | drop --all]
   park [list/show/pop/apply/drop/clear]
   retest run [--full] [--save] [-- <nextest args>], retest init [--yes], retest doctor
   tag, merge, pick, invert, replay, undo, op, try, bisect run, adopt
   ref set/list/log/delete, release attach/files/get, attest, attestations, change list/show/log/diff/edit/close/reopen/numbering
   view [<rev or line>] [--json], view save [<line>], view show <id>
Review changes:
   comment <change> -m <text> [--file <path> --line <a>[-<b>]] [--state <rev>]
   comment reply <comment> -m <text>, comment resolve <comment>, comment unresolve <comment>
   react <comment or change> <name> [--remove], review request <change> <reviewer>..., review cancel <change> <reviewer>...
Remotes:
   person keys, show, accept
   remote add, list, self, remove, trust, grant; sync, fetch, pull, push, clone; login
   index write [<remote>] [--json]; push <remote> --index
   mount <remote> [--listen 127.0.0.1:8095]
LayrFS (tree tools of agents from an in-memory index):
   fs serve, fs warm [<dir>] [--wait], fs status, fs stop
   tool rg|find [<args>]; or a link named rg or find to layr
Git bridge:
   git remote, fetch, pull, push, clone, sync
   queue add/list/remove/run
   follow [<remote>/<branch> | --run | --tick | --remove]
Overlays (files with their own readers and history, shown in lines):
   overlay create, set, list, readers, use, move, remote, log, show, diff, grep, ls-files
Projects and machines:
   init [--allow-root-snapshots], projects, project delete <project>, config, export, gc, du [<line>...] [--json], fsck [--caches], reindex,
   usage [--days <n>] [--json]
   daemon [--allow-root-snapshots] [--manual-queue]

Lines are folders: <root>/projects/<project>/lines/<line>. Run commands inside a line folder.
";

/// The commands that work in an overlay's own project: reading its history, and its checks.
const OVERLAY_READS: &[&str] =
    &["log", "show", "diff", "grep", "ls-files", "blame", "rev-parse", "merge-base", "fsck", "reindex", "help", "version"];

/// Public command names.
pub const COMMANDS: &[&str] = &[
    "help", "--help", "-h", "version", "--version", "status", "diff", "show", "log", "rev-parse", "merge-base", "blame",
    "index", "grep", "ls-files", "add", "rm", "mv", "apply", "restore", "reset", "retest", "park", "clean", "line", "tag", "adopt",
    "merge", "pick", "invert", "replay", "save", "undo", "op", "try", "bisect", "access", "protect", "approve", "checks", "ref",
    "release", "queue", "request", "requests", "view", "attest", "attestations", "change", "comment", "react", "review", "probe", "gc", "du",
    "fsck", "reindex", "init", "projects", "debug", "config", "remote", "fetch", "pull", "push", "git", "clone", "login", "overlay",
    "export", "sync", "serve-peer", "person", "mount", "daemon", "follow", "volume", "project",
    "usage", "keep", "objects",
    "migrate", "fs", "tool",
];

pub fn run(ctx: &Ctx, argv: &[String]) -> Result<i32> {
    crate::repo::begin_command();
    let (cmd, rest) = match argv.first() {
        Some(c) => (c.as_str(), &argv[1..]),
        None => {
            ctx.print(HELP);
            return Ok(1);
        }
    };
    // A dry run and a check only report: they leave the temporary copies of dead processes to the next command.
    let reports_only = cmd == "fsck" || rest.iter().any(|a| a == "-n" || a == "--dry-run");
    crate::store::SWEEP_TEMPORARIES.store(!reports_only, std::sync::atomic::Ordering::SeqCst);
    // An overlay (`<project>%<overlay>`) is read through layr only: its history, by its readers.
    if ctx.env("LAYR_PROJECT").map(|p| p.contains('%')).unwrap_or(false) {
        let reading = OVERLAY_READS.contains(&cmd)
            || (cmd == "op" && matches!(rest.first().map(|s| s.as_str()), None | Some("log") | Some("show")));
        if !reading {
            return Err(exit(1, format!("layr {cmd} is not available in an overlay; see 'layr overlay'")));
        }
    }
    // A virtual line (docs/views.md) takes only the commands that support it.
    crate::views::guard(ctx, cmd)?;
    match cmd {
        "help" | "--help" | "-h" => {
            ctx.print(HELP);
            Ok(0)
        }
        "version" | "--version" => {
            outln!(ctx, "{}", version_line());
            Ok(0)
        }
        "status" => work::status(ctx, rest),
        "diff" => history::diff(ctx, rest),
        "show" => history::show(ctx, rest),
        "log" => history::log(ctx, rest),
        "rev-parse" => history::rev_parse(ctx, rest),
        "merge-base" => history::merge_base(ctx, rest),
        "blame" => history::blame(ctx, rest),
        "index" => remote::index(ctx, rest),
        "grep" => history::grep(ctx, rest),
        "ls-files" => history::ls_files(ctx, rest),
        "add" => work::add(ctx, rest),
        "rm" => work::rm(ctx, rest),
        "mv" => work::mv(ctx, rest),
        "apply" => work::apply(ctx, rest),
        "restore" => work::restore(ctx, rest),
        "reset" => work::reset(ctx, rest),
        "park" => park::park(ctx, rest),
        "clean" => work::clean(ctx, rest),
        "line" => branch::line(ctx, rest),
        "tag" => branch::tag(ctx, rest),
        "adopt" => branch::adopt(ctx, rest),
        "merge" => mergecmd::merge(ctx, rest),
        "pick" => mergecmd::cherry_pick(ctx, rest),
        "invert" => mergecmd::revert(ctx, rest),
        "replay" => mergecmd::rebase(ctx, rest),
        "save" => admin::save(ctx, rest),
        "keep" => admin::keep(ctx, rest),
        "undo" => admin::undo(ctx, rest),
        "op" => admin::op(ctx, rest),
        "try" => admin::try_cmd(ctx, rest),
        "bisect" => bisect::run(ctx, rest),
        "access" => admin::access(ctx, rest),
        "protect" => admin::protect(ctx, rest),
        "approve" => admin::approve(ctx, rest),
        "checks" => checks::checks(ctx, rest),
        "ref" => refs::refs(ctx, rest),
        "release" => release::release(ctx, rest),
        "queue" => queue::queue(ctx, rest),
        "request" => requests::request(ctx, rest),
        "requests" => requests::requests(ctx, rest),
        "view" => view::view(ctx, rest),
        "attest" => attest::attest(ctx, rest),
        "attestations" => attest::attestations(ctx, rest),
        "change" => change::change(ctx, rest),
        "comment" => review::comment(ctx, rest),
        "react" => review::react(ctx, rest),
        "review" => review::review(ctx, rest),
        "probe" => probe::probe(ctx, rest),
        "gc" => admin::gc(ctx, rest),
        "du" => du::du(ctx, rest),
        "fsck" => admin::fsck(ctx, rest),
        "reindex" => admin::reindex(ctx, rest),
        "init" => admin::init(ctx, rest),
        "projects" => admin::projects(ctx, rest),
        "debug" => admin::debug(ctx, rest),
        "config" => admin::config(ctx, rest),
        "remote" => remote::remote(ctx, rest),
        "follow" => followcmd::follow(ctx, rest),
        "volume" => volume::volume(ctx, rest),
        "project" => project::project(ctx, rest),
        "usage" => crate::usage::show(ctx, rest),
        "fetch" => remote::fetch(ctx, rest),
        "pull" => remote::pull(ctx, rest),
        "push" => remote::push(ctx, rest),
        "git" => remote::git::run(ctx, rest),
        "clone" => remote::clone(ctx, rest),
        "login" => remote::login(ctx, rest),
        "overlay" => overlaycmd::overlay(ctx, rest),
        "objects" => objects::objects(ctx, rest),
        "migrate" => migrate::migrate(ctx, rest),
        "export" => transfer::export(ctx, rest),
        "sync" => remote::sync(ctx, rest),
        "serve-peer" => replicate::serve(ctx, rest),
        other => Err(exit(1, format!("layr: '{other}' is not a layr command. See 'layr help'."))),
    }
}

/// The text of `layr version`: the package version, then the build identity (LAYR_BUILD_ID from
/// build.rs) and the build date (LAYR_BUILD_DATE from build.rs).
pub fn version_line() -> String {
    format!("layr version {} ({} {})", env!("CARGO_PKG_VERSION"), env!("LAYR_BUILD_ID"), env!("LAYR_BUILD_DATE"))
}

#[cfg(test)]
mod tests {
    use super::{glob, version_line};

    #[test]
    fn version_line_names_the_build() {
        let shape = format!(
            r"^layr version {} \((dev|[0-9a-f]{{7,40}}) (dev|\d{{4}}-\d{{2}}-\d{{2}})\)$",
            regex::escape(env!("CARGO_PKG_VERSION"))
        );
        let line = version_line();
        assert!(regex::Regex::new(&shape).unwrap().is_match(&line), "{line}");
    }

    #[test]
    fn glob_matches() {
        assert!(glob("*.rs", "src/main.rs"));
        assert!(glob("src/*", "src/a/b"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("[a-c]x", "bx"));
        assert!(!glob("[!a-c]x", "bx"));
        assert!(glob("*a*b*c", "xxaxxbxxc"));
        assert!(!glob("*a*b*c", "xxaxxbxx"));
        assert!(glob("[", "["));
        assert!(glob("", ""));
        assert!(!glob("", "a"));
        assert!(glob("**", ""));
        // A pattern that takes exponential time with naive recursion.
        let text = "a".repeat(60);
        let start = std::time::Instant::now();
        assert!(!glob(&("*a".repeat(30) + "b"), &text));
        assert!(start.elapsed().as_secs() < 1);
    }
}
