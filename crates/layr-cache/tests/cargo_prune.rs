//! The cargo adapter on a real cargo project. Offline: the project has no dependencies.

use layr_cache::{cargo, prune, LocalHost, Outcome};

fn host() -> LocalHost {
    LocalHost::new(std::time::Duration::from_secs(600))
}
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// An empty folder under the temp dir, named for the test.
fn tmp(name: &str) -> PathBuf {
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-cache-cargo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// A one-crate project with the feature `extra`. `sweep` is the `sweep = [...]` line of the workspace
/// metadata, or None for no metadata. Returns the path of its `target` folder.
fn project(dir: &Path, sweep: Option<&str>) -> PathBuf {
    let meta = sweep.map(|s| format!("\n[workspace.metadata.layr-retest]\n{s}\n")).unwrap_or_default();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"prunews\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n\n[workspace]\n{meta}"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 {\n    1\n}\n").unwrap();
    dir.join("target")
}

/// Runs cargo in `dir` with `target` as its target folder. Returns stderr, where cargo writes `Compiling`.
fn cargo_in(dir: &Path, target: &Path, args: &[&str]) -> String {
    let o = Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    assert!(o.status.success(), "cargo {args:?} failed:\n{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Every file and folder under `dir`, as paths relative to it. Cargo's own cache of the rustc version
/// (`.rustc_info.json`) is left out: any cargo call can write it, the listing of a prune too.
fn names(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".rustc_info.json" {
                continue;
            }
            out.insert(p.strip_prefix(dir).unwrap().to_path_buf());
            if p.is_dir() {
                todo.push(p);
            }
        }
    }
    out
}

#[test]
fn removes_a_stale_unit_and_the_next_build_compiles_nothing() {
    let dir = tmp("stale");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let standard = names(&target);
    // The feature build makes units that no standard command uses.
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "extra"]);
    let with_feature = names(&target);
    assert!(with_feature.len() > standard.len());

    match prune(None, &target).unwrap().1 {
        Outcome::Pruned { freed } => {
            assert!(freed > 0);
        }
        other => panic!("expected a prune, got {other:?}"),
    }
    let after = names(&target);
    assert!(after.len() < with_feature.len(), "nothing was removed");
    assert!(after.is_subset(&with_feature));
    let stderr = cargo_in(&dir, &target, &["build", "--workspace"]);
    assert!(!stderr.contains("Compiling"), "{stderr}");
}

/// The cargo hash of the unit of `target/debug` whose fingerprint lists the feature `extra` (`extra`) or none.
fn unit_hash(target: &Path, extra: bool) -> String {
    let fingerprints = target.join("debug/.fingerprint");
    let unit = std::fs::read_dir(&fingerprints)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            let fingerprint: serde_json::Value =
                serde_json::from_slice(&std::fs::read(p.join("lib-prunews.json")).unwrap()).unwrap();
            (fingerprint["features"] == "[\"extra\"]") == extra
        })
        .expect("the unit of the feature build");
    unit.file_name().unwrap().to_string_lossy().rsplit_once('-').unwrap().1.to_string()
}

#[test]
fn skips_a_folder_with_a_stale_standard_unit_and_keeps_the_feature_unit() {
    let dir = tmp("dirty");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "extra"]);
    // The source change makes the standard unit stale: the listing stops, and the feature unit stays.
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 {\n    2\n}\n").unwrap();
    let before = names(&target);
    let standard = unit_hash(&target, false);

    match prune(None, &target).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("not fresh"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    // Cargo's freshness check removes the outputs of the stale unit itself (the next build rebuilds them), and
    // nothing else: every removed entry belongs to the standard unit.
    let after = names(&target);
    let removed: Vec<_> = before.difference(&after).collect();
    assert!(removed.iter().all(|p| p.to_string_lossy().contains(&standard)), "{removed:?}");
    assert!(after.iter().any(|p| p.to_string_lossy().contains(&unit_hash(&target, true))));
}

#[test]
fn skips_a_project_without_a_sweep_list() {
    let dir = tmp("nolist");
    let target = project(&dir, None);
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let before = names(&target);
    match prune(None, &target).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("sweep"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    assert_eq!(names(&target), before);
}

#[test]
fn skips_a_folder_that_is_not_named_target_and_a_target_without_output() {
    let dir = tmp("names");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    std::fs::create_dir_all(dir.join("out")).unwrap();
    match cargo::prune(&host(), &dir.join("out")) {
        Outcome::Skipped { reason } => assert!(reason.contains("named target"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    match cargo::prune(&host(), &target) {
        Outcome::Skipped { reason } => assert!(reason.contains("no build output"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
}

/// The user's env may set `CARGO_TERM_COLOR=always`: cargo then puts escape codes in front of its status lines.
/// The listing must still see the `Blocking` line of a busy build lock, and skip within its deadline.
#[test]
fn a_busy_build_lock_skips_within_the_deadline_even_with_colors() {
    let dir = tmp("busy-color");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    // Another cargo holds the build lock of the profile folder.
    let lock = std::fs::File::options().write(true).open(target.join("debug/.cargo-lock")).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    std::env::set_var("CARGO_TERM_COLOR", "always");

    let started = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(10);
    let outcome = cargo::prune(&layr_cache::LocalHost::new(deadline), &target);
    std::env::remove_var("CARGO_TERM_COLOR");
    match outcome {
        Outcome::Skipped { reason } => assert!(reason.contains("build lock busy"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(started.elapsed() < deadline - std::time::Duration::from_secs(2), "the sweep waited for the deadline: {:?}", started.elapsed());
}

#[test]
fn warm_builds_the_sweep_commands_so_the_next_build_compiles_nothing() {
    let dir = tmp("warm");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"], [\"test\", \"--workspace\", \"--no-run\"]]"));
    cargo::warm(&host(), &target).unwrap();
    for args in [&["build", "--workspace"][..], &["test", "--workspace", "--no-run"]] {
        let stderr = cargo_in(&dir, &target, args);
        assert!(!stderr.contains("Compiling"), "{args:?}: {stderr}");
    }
    // A project without a sweep list has nothing to build.
    let bare = tmp("warm-bare");
    let t = project(&bare, None);
    assert!(cargo::warm(&host(), &t).is_err());
    assert!(!t.exists(), "nothing was built");
}

/// A helper process that takes an exclusive flock on `path` and prints `locked`. It is killed on drop.
struct Holder {
    child: std::process::Child,
}

impl Holder {
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", self.child.id())]).status();
        let _ = self.child.wait();
    }
}

fn hold(path: &Path) -> Holder {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("perl")
        .args([
            "-MFcntl=:flock",
            "-e",
            "$| = 1; open my $f, '>>', $ARGV[0] or die $!; flock($f, LOCK_EX) or die $!; print \"locked\\n\"; sleep 600",
        ])
        .arg(path)
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
    assert_eq!(line, "locked\n");
    Holder { child }
}

/// The busy build lock is named with the process that holds it: the reason of the skip has its pid.
#[test]
fn busy_build_lock_names_its_holder() {
    let dir = tmp("busy-holder");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let holder = hold(&target.join("debug/.cargo-lock"));
    match cargo::prune(&host(), &target) {
        Outcome::Skipped { reason } => assert!(reason.contains(&format!("pid {} (", holder.pid())), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
}

/// A host whose cargo is a script. The script waits for the package cache lock and leaves a process in its
/// own session, which keeps the output pipes open.
struct Script {
    path: PathBuf,
    until: std::time::Instant,
}

impl layr_adapter::Host for Script {
    fn command(&self, _program: &str) -> std::io::Result<Command> {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(&self.path);
        c.process_group(0);
        Ok(c)
    }

    fn deadline(&self) -> std::time::Instant {
        self.until
    }
}

/// A listing that waits for the package cache past its time limit is killed, and its error names the holder.
/// The output pipes stay open in a process outside the group: the listing still returns at the limit.
#[test]
fn listing_past_its_time_limit_is_killed_and_names_the_holder() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("waits");
    let target = dir.join("target");
    let home = dir.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    let holder = hold(&home.join(".package-cache"));
    let daemon = dir.join("daemon.pid");
    let script = dir.join("fake-cargo.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho '    Blocking waiting for file lock on package cache' >&2\n\
         perl -MPOSIX -e 'POSIX::setsid(); exec \"sleep\", \"600\"' &\n\
         echo $! > \"$DAEMON_PID\"\nsleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let host = Script { path: script, until: std::time::Instant::now() + std::time::Duration::from_secs(2) };
    let env = vec![
        ("CARGO_HOME".to_string(), home.display().to_string()),
        ("DAEMON_PID".to_string(), daemon.display().to_string()),
    ];
    let (root, target2) = (dir.clone(), target.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(cargo::list_units(&host, &root, &target2, &env, &["build".to_string(), "--workspace".to_string()]));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(60));
    // The process that the script left behind is stopped whatever the result.
    if let Ok(pid) = std::fs::read_to_string(&daemon) {
        let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
    }
    let result = result.expect("the listing did not return: its output pipes kept it open past the limit");
    let msg = match result {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a listing past its limit is an error"),
    };
    assert!(msg.contains("time limit") && msg.contains("package cache"), "{msg}");
    assert!(msg.contains(&format!("pid {} (", holder.pid())), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "{:?}", started.elapsed());
    drop(holder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sets the modification time of `path` and of everything under it to 2001-01-01.
fn make_old(path: &Path) {
    let st = Command::new("find").arg(path).args(["-exec", "touch", "-h", "-t", "200101010000", "{}", "+"]).status().unwrap();
    assert!(st.success());
}

#[test]
fn an_unlisted_folder_goes_only_when_no_build_wrote_it_for_a_week() {
    let target = tmp("unlisted").join("target");
    for d in ["debug/deps", "release/deps", "retest/debug", "profraw/other", "x86_64-unknown-linux-gnu/debug", "doc/prunews", "custom/deps", "aarch64-apple-darwin/debug"] {
        std::fs::create_dir_all(target.join(d)).unwrap();
        std::fs::write(target.join(d).join("f"), "x").unwrap();
    }
    std::fs::write(target.join("CACHEDIR.TAG"), "x").unwrap();
    let commands: Vec<Vec<String>> = [vec!["build"], vec!["build", "--release"], vec!["build", "--target", "aarch64-apple-darwin"]]
        .iter()
        .map(|c| c.iter().map(|s| s.to_string()).collect())
        .collect();
    let now = std::time::SystemTime::now();
    assert!(cargo::stale_unlisted(&target, &commands, now).is_empty(), "a folder written now stays");
    let week = std::time::Duration::from_secs(8 * 24 * 3600);
    let stale: Vec<String> = cargo::stale_unlisted(&target, &commands, now + week)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stale, ["custom", "doc", "x86_64-unknown-linux-gnu"]);
}

#[test]
fn prune_removes_an_old_unlisted_folder_and_keeps_a_new_one() {
    let dir = tmp("unlisted-prune");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    for d in ["doc/prunews", "x86_64-unknown-linux-gnu/debug"] {
        std::fs::create_dir_all(target.join(d)).unwrap();
        std::fs::write(target.join(d).join("big"), vec![0u8; 4096]).unwrap();
    }
    make_old(&target.join("doc"));
    match prune(None, &target).unwrap().1 {
        Outcome::Pruned { freed } => assert!(freed >= 4096, "freed {freed}"),
        other => panic!("expected a prune, got {other:?}"),
    }
    assert!(!target.join("doc").exists(), "the old unlisted folder goes");
    assert!(target.join("x86_64-unknown-linux-gnu/debug/big").exists(), "a folder a build wrote this week stays");
    let stderr = cargo_in(&dir, &target, &["build", "--workspace"]);
    assert!(!stderr.contains("Compiling"), "{stderr}");
}
