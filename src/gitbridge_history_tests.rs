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
        let tmp = Command::new("mktemp").args(["-d", "-t", "layr-history.XXXXXXXX"]).output()?;
        anyhow::ensure!(tmp.status.success(), "mktemp failed");
        let root = PathBuf::from(std::str::from_utf8(&tmp.stdout)?.trim());
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
            out: RefCell::new(Box::new(output.clone())),
        };
        let p = Project {
            store: &ctx.store,
            name: "history".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: crate::db::Db::open(&db_file(&dir))?,
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"name": "history", "id": p.id, "config": {}});
        p.append(record, &ctx.actor())?;
        let repo = Repo { p, ctx: Some(&ctx) };
        let git = repo.p.git_dir();
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        let run = |args: &[&str]| -> Result<String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["-c", "user.name=History Test", "-c", "user.email=history@example.com"])
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_WORK_TREE")
                .env("LAYR_ROOT", &ctx.store.root)
                .output()?;
            anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            Ok(String::from_utf8(out.stdout)?.trim().to_string())
        };
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
            repo.p.append(rec, &ctx.actor())?;
            states = vec![id];
        }
        let state = states.pop().unwrap();
        let base_state = repo.state(&state)?.parents[0].clone();
        drop(repo);
        Ok(Self { ctx, output, root, commit, parent, state, base_state })
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
        let changes = sides.changes.into_iter().filter(|c| crate::cmd::matches(&specs, &c.path)).collect::<Vec<_>>();
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
