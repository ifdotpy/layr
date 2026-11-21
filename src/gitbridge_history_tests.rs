//! Imported history reads use an ordinary temporary Git repository and database: no snapshots,
//! daemon, real store, or integration-test harness is involved.
use super::*;
use crate::cmd::history::{log, show, Entries};
use crate::ctx::Caller;
use crate::store::{db_file, Project, Store};
use std::cell::RefCell;
use std::rc::Rc;

const FILE_COUNT: usize = 128;
const FILE_BYTES: usize = 4096;

#[derive(Clone, Default)]
struct Output(Rc<RefCell<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Fixture {
    ctx: Ctx,
    output: Output,
    root: PathBuf,
    commit: String,
    parent: String,
    state: String,
    base_state: String,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("layr-history-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        let store = Store::open(&root.join("store"))?;
        let dir = store.root.join("projects/history");
        std::fs::create_dir_all(dir.join("meta/db"))?;
        std::fs::create_dir_all(dir.join("meta/work"))?;
        let output = Output::default();
        let ctx = Ctx {
            store,
            caller: Caller::from_uid(rustix::process::getuid().as_raw()),
            cwd: dir.clone(),
            env: [("LAYR_PROJECT".into(), "history".into()), ("LAYR_ROOT".into(), root.join("store").display().to_string())]
                .into(),
            author: Default::default(),
            captures: Default::default(),
            out: RefCell::new(Box::new(output.clone())),
        };
        let p = Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "history".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: crate::db::Db::open(&db_file(&dir))?,
        };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"name": "history", "id": p.id, "config": {}});
        p.lock_maintenance().unwrap().append(record, &ctx.actor())?;
        let repo = Repo { restore_from_remote: true, p, ctx: Some(&ctx) };
        let git = repo.p.git_dir();
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        let run = |args: &[&str]| source_git(&root, args);
        run(&["init", "-q"])?;
        std::fs::write(source.join("seed"), b"original\n")?;
        std::fs::create_dir(source.join("move-out"))?;
        std::fs::write(source.join("move-out/file"), b"moving out\n")?;
        std::fs::write(source.join("move-in"), b"moving in\n")?;
        run(&["add", "."])?;
        run(&["commit", "-qm", "base"])?;
        let parent = run(&["rev-parse", "HEAD"])?;
        std::fs::create_dir(source.join("selected"))?;
        std::fs::create_dir(source.join("other"))?;
        for n in 0..FILE_COUNT {
            let folder = if n < FILE_COUNT / 2 { "selected" } else { "other" };
            let mut data = format!("file {n}\n").into_bytes();
            data.resize(FILE_BYTES, b'x');
            std::fs::write(source.join(format!("{folder}/{n:03}")), data)?;
        }
        std::fs::write(source.join("seed"), b"modified\n")?;
        run(&["mv", "move-out/file", "moved-out"])?;
        run(&["mv", "move-in", "selected/moved-in"])?;
        std::os::unix::fs::symlink("001", source.join("selected/link"))?;
        std::fs::write(source.join("selected/binary"), b"\0\xffbinary\n")?;
        std::fs::set_permissions(source.join("selected/001"), std::fs::Permissions::from_mode(0o755))?;
        run(&["add", "."])?;
        run(&["commit", "-qm", "many changes"])?;
        let commit = run(&["rev-parse", "HEAD"])?;
        run(&["clone", "--bare", "-q", source.to_str().unwrap(), git.to_str().unwrap()])?;
        let mut cat = CatFile::new(&git)?;
        let mut states = Vec::new();
        for (oid, old) in [(&parent, None), (&commit, Some(parent.as_str()))] {
            let raw = cat.object(oid, "commit")?;
            let (objects, changes) = import_changes(&raw, old, &mut cat)?;
            let (id, rec) = import_record(&ctx, &repo, oid, &raw, "origin/main", states.clone(), objects, changes)?;
            repo.p.lock_maintenance().unwrap().append(rec, &ctx.actor())?;
            states = vec![id];
        }
        let state = states.pop().unwrap();
        let base_state = repo.state(&state)?.parents[0].clone();
        drop(repo);
        Ok(Self { ctx, output, root, commit, parent, state, base_state })
    }

    /// Commits `count` more Git commits on top of the fixture and imports them as Git-only states.
    /// Even commits add a line to `seed`; odd ones change the last file of `other` only, so `seed`
    /// keeps the blob of the commit before.
    fn extend(&mut self, count: usize) -> Result<()> {
        let source = self.root.join("source");
        for n in 0..count {
            let file = if n % 2 == 0 { source.join("seed") } else { source.join(format!("other/{:03}", FILE_COUNT - 1)) };
            writeln!(std::fs::OpenOptions::new().append(true).open(file)?, "line {n}")?;
            source_git(&self.root, &["commit", "-qam", &format!("extend {n}")])?;
        }
        let repo = self.repo()?;
        let git = repo.p.git_dir();
        source_git(&self.root, &["push", "-q", &git.to_string_lossy(), "HEAD:refs/heads/main"])?;
        let commit = source_git(&self.root, &["rev-parse", "HEAD"])?;
        let state = import_commit(&self.ctx, &repo, &commit, "origin/main")?.0;
        drop(repo);
        (self.commit, self.state) = (commit, state);
        Ok(())
    }

    /// `layr blame <state> -- <path>` with a fresh object reader: the output, the git commands
    /// it started and the blobs it read.
    fn blame(&self, extra: &[&str], path: &str) -> Result<(String, usize, usize)> {
        forget_objects();
        self.output.0.borrow_mut().clear();
        let before = GIT_COMMANDS.with(|n| n.get());
        let mut args: Vec<String> = extra.iter().map(|a| a.to_string()).collect();
        args.extend([self.state.clone(), "--".into(), path.into()]);
        let result = crate::cmd::history::blame(&self.ctx, &args);
        let commands = GIT_COMMANDS.with(|n| n.get()) - before;
        crate::free::finish(&self.ctx);
        result?;
        Ok((String::from_utf8(self.output.0.borrow().clone())?, commands, metrics().0))
    }

    fn repo(&self) -> Result<Repo<'_>> {
        Repo::open(&self.ctx, "history")
    }

    fn command(&self, log_command: bool, specs: &[&str], patch: bool) -> Result<Vec<u8>> {
        forget_objects();
        self.output.0.borrow_mut().clear();
        let revision = if log_command { format!("{}..{}", self.base_state, self.state) } else { self.state.clone() };
        let mut args = vec!["--format=".into(), revision];
        if patch {
            args.push("-p".into());
        }
        if !specs.is_empty() {
            args.push("--".into());
            args.extend(specs.iter().map(|s| s.to_string()));
        }
        let result = if log_command { log(&self.ctx, &args) } else { show(&self.ctx, &args) };
        crate::free::finish(&self.ctx);
        result?;
        Ok(self.output.0.borrow().clone())
    }

    fn baseline(&self, specs: &[&str]) -> Result<Vec<u8>> {
        let repo = self.repo()?;
        let deltas = deltas_of(&repo, Some(&self.parent), &self.commit)?;
        let sides = sides_of(&repo, &deltas)?;
        let specs = specs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let changes = sides.changes.into_iter().filter(|c| crate::pathspec::matches(&specs, c.path())).collect::<Vec<_>>();
        let diffs = crate::diff::file_diffs(&Entries::Git(sides.old), &Entries::Git(sides.new), &changes)?;
        // Match the command's default unified context.
        const CONTEXT_LINES: usize = 3;
        Ok(diffs.iter().map(|diff| crate::diff::patch(diff, CONTEXT_LINES)).collect::<String>().into_bytes())
    }

    fn object(&self, path: &str) -> Result<PathBuf> {
        let git = self.repo()?.p.git_dir();
        let oid = git_out(&git, &["rev-parse", &format!("{}:{path}", self.commit)])?;
        let oid = oid.trim();
        Ok(git.join("objects").join(&oid[..2]).join(&oid[2..]))
    }

    /// Exercise the filesystem/object history branch with the same rename fixture as Git.
    fn object_history(&mut self) -> Result<()> {
        let repo = self.repo()?;
        let store = crate::objstore::open(&repo.p)?;
        let source = self.root.join("source");
        let mut parent = None;
        for (oid, id) in [(&self.parent, &self.base_state), (&self.commit, &self.state)] {
            source_git(&self.root, &["checkout", "-q", oid])?;
            let report = layr_objects::write_tree(
                &store,
                &source,
                &layr_objects::WriteOptions {
                    temperature: layr_objects::Temperature::Hot,
                    previous: None,
                    exclude: vec![b".git".to_vec()],
                    similar: false,
                },
            )?;
            let mut state = repo.state(id)?;
            state.id = ids::new_id();
            state.kind = StateKind::Commit;
            state.parents = parent.into_iter().collect();
            state.tree = Some(report.tree.unwrap().hex());
            state.times = Some(report.times.unwrap().hex());
            parent = Some(state.id.clone());
            let mut record = crate::model::DraftRecord::new("save");
            record.states.push(state.clone());
            repo.p.lock_maintenance().unwrap().append(record, &self.ctx.actor())?;
            // Mark content as locally readable without requiring native snapshots. state_src
            // still selects the verified objects, as it does for a migrated state.
            std::fs::create_dir_all(repo.p.state_path(&state.id))?;
        }
        let state = parent.unwrap();
        let base = repo.state(&state)?.parents[0].clone();
        drop(repo);
        self.state = state;
        self.base_state = base;
        Ok(())
    }
}

/// A git command in the source repository of the fixture at `root`; its trimmed output.
fn source_git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root.join("source"))
        .args(["-c", "user.name=History Test", "-c", "user.email=history@example.com"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env("LAYR_ROOT", root.join("store"))
        .output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

impl Drop for Fixture {
    fn drop(&mut self) {
        forget_objects();
        crate::free::finish(&self.ctx);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn metrics() -> (usize, usize) {
    OBJECTS.with(|slot| {
        let slot = slot.borrow();
        let cat = &slot.as_ref().unwrap().1;
        (cat.blob_requests, cat.decoded_blob_bytes)
    })
}

#[test]
fn filtered_history_matches_load_then_filter() -> Result<()> {
    let fixture = Fixture::new()?;
    for specs in [
        vec!["selected"],
        vec!["selected/00?"],
        vec!["move-out"],
        vec!["moved-out"],
        vec!["move-in"],
        vec!["selected/moved-in"],
        vec!["absent"],
        vec!["seed", "selected/001"],
        vec![],
    ] {
        let expected = fixture.baseline(&specs)?;
        assert_eq!(fixture.command(false, &specs, true)?, expected, "show {specs:?}");
        assert_eq!(fixture.command(true, &specs, true)?, expected, "log -p {specs:?}");
    }
    Ok(())
}

#[test]
fn filtered_history_counts_blob_requests_and_bytes() -> Result<()> {
    let fixture = Fixture::new()?;
    forget_objects();
    let repo = fixture.repo()?;
    let deltas = deltas_of(&repo, Some(&fixture.parent), &fixture.commit)?;
    assert!(!deltas.is_empty());
    assert_eq!(metrics(), (0, 0), "tree delta discovery must not request blobs");
    fixture.command(false, &[], true)?;
    let all = metrics();
    for log_command in [false, true] {
        fixture.command(log_command, &["selected/001"], false)?;
        assert_eq!(metrics(), (1, FILE_BYTES));
    }
    fixture.command(true, &["selected/001"], true)?;
    assert_eq!(metrics(), (2, 2 * FILE_BYTES), "log -p validates during discovery and loads again for rendering");
    fixture.command(false, &["absent"], true)?;
    assert_eq!(metrics(), (0, 0));
    fixture.command(false, &["seed"], true)?;
    assert_eq!(metrics(), (2, b"original\n".len() + b"modified\n".len()));
    fixture.command(false, &["selected/00?"], true)?;
    assert_eq!(metrics(), (10, 10 * FILE_BYTES));
    eprintln!(
        "generated commit: files={FILE_COUNT}, file_bytes={FILE_BYTES}, unfiltered={all:?}, selected={:?}, isolated_store={}",
        (1, FILE_BYTES),
        fixture.ctx.store.root.display()
    );
    Ok(())
}

#[test]
fn filtered_history_ignores_only_unselected_damaged_blobs() -> Result<()> {
    let fixture = Fixture::new()?;
    let damaged_path = format!("other/{:03}", FILE_COUNT - 1);
    let damaged = fixture.object(&damaged_path)?;
    let original = std::fs::read(&damaged)?;
    for missing in [false, true] {
        std::fs::set_permissions(&damaged, std::fs::Permissions::from_mode(0o644))?;
        if missing {
            std::fs::remove_file(&damaged)?;
        } else {
            // A readable loose object with the wrong hash exercises our integrity check.
            let mut encoded = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoded.write_all(b"blob 7\0corrupt")?;
            std::fs::write(&damaged, encoded.finish()?)?;
        }
        for log_command in [false, true] {
            fixture.command(log_command, &["selected/001"], true)?;
            assert!(fixture.command(log_command, &[&damaged_path], true).is_err());
            assert!(fixture.command(log_command, &[&damaged_path], false).is_err());
            assert!(fixture.command(log_command, &[], true).is_err());
        }
        std::fs::write(&damaged, &original)?;
    }
    Ok(())
}

#[test]
fn object_path_history_keeps_rename_endpoints_without_reading_unrelated_changes() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.object_history()?;
    let repo = fixture.repo()?;
    let root = repo.state_src(&fixture.state)?;
    let tree = crate::fsutil::Src::objects(&root).unwrap();
    let layr_objects::Node::File { blob, .. } = tree.entry("seed")?.unwrap().entry.node else { unreachable!() };
    let blob_path = tree.store.hot_path(&blob).unwrap();
    let bytes = std::fs::read(&blob_path)?;
    // A changed, same-size file outside every selected rename path. Reading its bytes must
    // fail, so success below proves the history scan skips it before content comparison.
    std::fs::remove_file(&blob_path)?;
    for path in ["selected/001", "move-out", "moved-out", "move-in", "selected/moved-in", "selected/00?"] {
        fixture.output.0.borrow_mut().clear();
        let result = log(
            &fixture.ctx,
            &["--format=%s".into(), format!("{}..{}", fixture.base_state, fixture.state), "--".into(), path.into()],
        );
        crate::free::finish(&fixture.ctx);
        result?;
        assert_eq!(&*fixture.output.0.borrow(), b"many changes\n", "log -- {path}");
    }
    fixture.output.0.borrow_mut().clear();
    let result = log(
        &fixture.ctx,
        &["--format=%s".into(), format!("{}..{}", fixture.base_state, fixture.state), "--".into(), "absent".into()],
    );
    crate::free::finish(&fixture.ctx);
    result?;
    assert!(fixture.output.0.borrow().is_empty(), "log -- absent selects no state");
    assert!(fixture.command(true, &["seed"], false).is_err(), "selected damage is still reported");
    std::fs::write(blob_path, bytes)?;
    for path in ["move-out", "moved-out", "move-in", "selected/moved-in"] {
        assert_eq!(fixture.command(true, &[path], true)?, fixture.command(false, &[path], true)?);
    }
    Ok(())
}

#[test]
fn blame_of_imported_history_starts_a_fixed_number_of_git_commands() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let (short, short_commands, short_blobs) = fixture.blame(&[], "seed")?;
    assert_eq!(short.lines().count(), 1, "{short}");
    assert_eq!(short_blobs, 2, "the two versions of seed");
    let second = fixture.state.clone();
    const EXTRA: usize = 40;
    fixture.extend(EXTRA)?;
    let (long, long_commands, long_blobs) = fixture.blame(&[], "seed")?;
    assert_eq!(long.lines().count(), 1 + EXTRA / 2, "{long}");
    assert!(long.lines().nth(1).is_some_and(|l| l.ends_with(") line 0")), "{long}");
    assert_eq!((short_commands, long_commands), (1, 1), "one object reader, whatever the length of the history");
    assert_eq!(long_blobs, 2 + EXTRA / 2, "one read per distinct blob of seed: {long}");
    let (range, ..) = fixture.blame(&["-L", "2,3"], "seed")?;
    assert_eq!(range, long.lines().skip(1).take(2).map(|l| format!("{l}\n")).collect::<String>());
    // The walk ends where the path is missing: selected/moved-in exists only from the second commit.
    let (moved, ..) = fixture.blame(&["-s"], "selected/moved-in")?;
    assert_eq!(moved.lines().count(), 1, "{moved}");
    assert!(moved.starts_with(&format!("{} ", &ids::display(&second)[..8])), "{moved}");
    // A link blames its target text; a folder or a missing path has no versions.
    let (link, ..) = fixture.blame(&["-s"], "selected/link")?;
    assert!(link.trim_end().ends_with(") 001"), "{link}");
    assert!(fixture.blame(&[], "selected").is_err());
    assert!(fixture.blame(&[], "absent").is_err());
    Ok(())
}

#[test]
fn tree_find_matches_whole_names_and_reads_folder_modes() -> Result<()> {
    let record = |mode: &str, name: &str, byte: u8| [format!("{mode} {name}\0").into_bytes(), vec![byte; 20]].concat();
    let tree = [
        record("100644", "a", 1),
        record("040000", "ab", 2),
        record("120000", "b", 3),
        record("0160000", "c", 4),
        record("40755", "d", 5),
    ]
    .concat();
    let found = |name: &str| tree_find(&tree, name.as_bytes()).map(|e| e.map(|e| (e.mode, e.oid)));
    for (name, mode, byte) in [("a", "100644", 1), ("ab", "40000", 2), ("b", "120000", 3), ("c", "160000", 4), ("d", "40000", 5)]
    {
        assert_eq!(found(name)?, Some((mode.to_string(), hex::encode([byte; 20]))), "{name}");
    }
    for absent in ["", "a/", "abc", "e", "100644"] {
        assert!(found(absent)?.is_none(), "{absent:?}");
    }
    assert!(tree_find(&tree[..tree.len() - 1], b"d").is_err(), "a short object ID");
    assert!(tree_find(b"100644 a", b"a").is_err(), "a name without its end");
    assert!(tree_find(b"100644", b"a").is_err(), "a mode without a name");
    assert!(tree_find(b"", b"a")?.is_none());
    assert!(tree_find(&[record("10064x", "a", 1), tree.clone()].concat(), b"b").is_err(), "a mode that is not octal");
    let parsed = parse_tree(&tree)?;
    assert_eq!(parsed.keys().collect::<Vec<_>>(), ["a", "ab", "b", "c", "d"]);
    assert!(parsed["ab"].is_tree() && parsed["d"].is_tree() && parsed["c"].is_link() && !parsed["b"].is_tree());
    assert!(parse_tree(&tree[..tree.len() - 1]).is_err());
    Ok(())
}

#[test]
fn canon_mode_reads_the_file_type_bits_as_git_does() {
    for (raw, mode) in [
        ("40000", Some(0o40000)),
        ("040000", Some(0o40000)),
        ("40755", Some(0o40000)),
        ("100644", Some(0o100644)),
        ("100664", Some(0o100644)),
        ("100755", Some(0o100755)),
        ("100744", Some(0o100755)),
        ("0100644", Some(0o100644)),
        ("120000", Some(0o120000)),
        ("120777", Some(0o120000)),
        ("160000", Some(0o160000)),
        ("160755", Some(0o160000)),
        ("0", Some(0o160000)),
        ("", None),
        ("8", None),
        ("10064x", None),
        ("+100644", None),
        ("400000100644", Some(0o100644)),
    ] {
        assert_eq!(canon_mode(raw.as_bytes()), mode, "{raw:?}");
    }
}

#[test]
fn paths_go_through_folders_with_padded_and_unusual_modes() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = fixture.repo()?;
    let git = repo.p.git_dir();
    let scratch = fixture.root.join("raw");
    std::fs::create_dir(&scratch)?;
    // Objects Git reads but would not write today: written as they are (`--literally`).
    let write = |kind: &str, data: &[u8]| -> Result<String> {
        let file = scratch.join(crate::ids::new_id());
        std::fs::write(&file, data)?;
        Ok(git_out(&git, &["hash-object", "-w", "--literally", "-t", kind, &file.to_string_lossy()])?.trim().to_string())
    };
    let raw_id = |oid: &str| hex::decode(oid).map_err(anyhow::Error::from);
    let file = write("blob", b"inside\n")?;
    let folder = write("tree", &[b"100644 x\0".as_slice(), &raw_id(&file)?].concat())?;
    for mode in ["40755", "040000"] {
        let root = write("tree", &[format!("{mode} src\0").as_bytes(), &raw_id(&folder)?].concat())?;
        let identity = ["-c", "user.name=History Test", "-c", "user.email=history@example.com"];
        let commit = git_out(&git, &[&identity[..], &["commit-tree", &root, "-m", mode]].concat())?.trim().to_string();
        assert!(matches!(entry(&repo, &commit, "src/x")?, Entry::File(data) if data == b"inside\n"), "{mode}");
        assert!(matches!(entry(&repo, &commit, "")?, Entry::Dir(names) if names == [("src".to_string(), true)]), "{mode}");
        assert_eq!(file_blob(&repo, &commit, "src/x")?, Some(file.clone()), "{mode}");
        assert_eq!(file_blob(&repo, &commit, "src")?, None, "{mode}");
    }
    Ok(())
}

#[test]
fn entry_reads_paths_of_an_imported_commit_through_one_reader() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = fixture.repo()?;
    forget_objects();
    let before = GIT_COMMANDS.with(|n| n.get());
    let at = |path: &str| entry(&repo, &fixture.commit, path);
    let folder = |entry: Entry| match entry {
        Entry::Dir(names) => names,
        _ => Vec::new(),
    };
    let root = folder(at("")?);
    assert_eq!(folder(at("/")?), root, "a slash names the root too");
    assert!(root.contains(&("selected".into(), true)) && root.contains(&("seed".into(), false)), "{root:?}");
    let selected = folder(at("selected/")?);
    assert!(selected.contains(&("link".into(), false)) && selected.windows(2).all(|w| w[0] < w[1]), "{selected:?}");
    assert!(matches!(at("selected/link")?, Entry::Link(target) if target == b"001"));
    assert!(matches!(at("seed")?, Entry::File(data) if data == b"modified\n"));
    assert!(matches!(entry(&repo, &fixture.parent, "seed")?, Entry::File(data) if data == b"original\n"));
    // Like Git, a path names its components literally: no empty or `.` component.
    for missing in ["absent", "seed/below", "selected/absent", "selected/00", "selected//001", "selected/./001"] {
        assert!(matches!(at(missing)?, Entry::Missing), "{missing}");
    }
    assert_eq!(GIT_COMMANDS.with(|n| n.get()) - before, 1, "one object reader for every read");
    // A read of the wrong type leaves the body of the object unread: the next read must not take
    // it for its answer.
    let folder = git_out(&repo.p.git_dir(), &["rev-parse", &format!("{}:selected", fixture.commit)])?;
    assert!(blob(&repo, folder.trim()).is_err());
    assert!(matches!(at("seed")?, Entry::File(data) if data == b"modified\n"));
    // A readable blob stored under the wrong ID is an error, not its bytes.
    let stored = fixture.object("selected/002")?;
    std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o644))?;
    let mut encoded = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoded.write_all(b"blob 7\0corrupt")?;
    std::fs::write(&stored, encoded.finish()?)?;
    assert!(at("selected/002").is_err());
    Ok(())
}

#[test]
fn object_of_names_the_object_git_names_and_reads_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = fixture.repo()?;
    let git = repo.p.git_dir();
    for path in ["", "seed", "selected", "selected/link", "selected/001"] {
        let expected = git_out(&git, &["rev-parse", &format!("{}:{path}", fixture.commit)])?;
        assert_eq!(object_of(&repo, &fixture.state, path)?.as_deref(), Some(expected.trim()), "{path:?}");
    }
    for absent in ["absent", "seed/below", "selected/absent"] {
        assert_eq!(object_of(&repo, &fixture.state, absent)?, None, "{absent}");
    }
    // The object of the path is read too: a missing blob is an error, not an ID.
    std::fs::remove_file(fixture.object("selected/002")?)?;
    assert!(object_of(&repo, &fixture.state, "selected/002").is_err());
    Ok(())
}
