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
fn a_stale_workspace_unit_keeps_every_workspace_unit() {
    let dir = tmp("dirty");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "extra"]);
    // A stale workspace unit and the feature unit both stay.
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 {\n    2\n}\n").unwrap();
    let before = names(&target);

    match prune(None, &target).unwrap().1 {
        Outcome::Pruned { .. } => {}
        other => panic!("expected a prune, got {other:?}"),
    }
    let after = names(&target);
    assert_eq!(before, after);
    assert!(after.iter().any(|p| p.to_string_lossy().contains(&unit_hash(&target, true))));
}

#[test]
fn listing_rejects_a_target_dir_argument() {
    let dir = tmp("target-argument");
    let target = project(&dir, None);
    let args = ["build".to_string(), "--target-dir".to_string(), target.display().to_string()];
    let error = match cargo::list_units(&host(), &dir, &target, &[], &args) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("the target-dir argument must be rejected"),
    };
    assert!(error.contains("--target-dir"), "{error}");
}

#[test]
fn listing_skips_a_target_with_a_symbolic_link() {
    let dir = tmp("target-link");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    std::os::unix::fs::symlink(dir.join("src"), target.join("link")).unwrap();
    let before = names(&target);
    match prune(None, &target).unwrap().1 {
        Outcome::Skipped { reason } => {
            assert!(reason.contains("links or special files"), "{reason}")
        }
        other => panic!("expected a skip, got {other:?}"),
    }
    assert_eq!(before, names(&target));
}

#[test]
fn listing_observes_the_lock_of_an_explicit_target() {
    let dir = tmp("target-lock");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    assert!(rustc.status.success());
    let version = String::from_utf8(rustc.stdout).unwrap();
    let triple = version.lines().find_map(|line| line.strip_prefix("host: ")).unwrap();
    cargo_in(&dir, &target, &["build", "--workspace", "--target", triple]);
    let lock = std::fs::File::options().write(true).open(target.join(triple).join("debug/.cargo-lock")).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let args = ["build".to_string(), "--workspace".to_string(), "--target".to_string(), triple.to_string()];
    match cargo::list_units(&host(), &dir, &target, &[], &args).unwrap() {
        cargo::Listed::Busy(_) => {}
        _ => panic!("the target profile is busy"),
    }
}

#[test]
fn skipped_prune_keeps_old_unlisted_folders() {
    let dir = tmp("dirty-unlisted");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let old = target.join("old-target");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("artifact"), "old output").unwrap();
    make_old(&old);
    std::fs::write(dir.join("Cargo.toml"), "[invalid\n").unwrap();
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Skipped { .. }));
    assert!(old.join("artifact").is_file(), "a skipped prune removed an unlisted folder");
}

#[test]
fn prunes_a_workspace_with_only_a_separate_target_dir() {
    let dir = tmp("only-other-dir");
    let target = project(&dir, Some("sweep = [{ target_dir = \"special\", args = [\"build\", \"--workspace\"] }]"));
    let special = target.join("special");
    cargo_in(&dir, &special, &["build", "--workspace"]);
    cargo_in(&dir, &special, &["build", "--workspace", "--features", "extra"]);
    let before = names(&special);
    match prune(None, &target).unwrap().1 {
        Outcome::Pruned { freed } => assert!(freed > 0),
        other => panic!("expected a prune, got {other:?}"),
    }
    assert!(names(&special).len() < before.len());
    assert!(!cargo_in(&dir, &special, &["build", "--workspace"]).contains("Compiling"));
}

#[test]
fn a_failed_separate_listing_does_not_prune_the_main_target() {
    let dir = tmp("other-listing-fails");
    let target = project(
        &dir,
        Some(
            "sweep = [[\"build\", \"--workspace\"], { target_dir = \"special\", args = [\"build\", \"--config=build.jobs=1\"] }]",
        ),
    );
    cargo_in(&dir, &target, &["build", "--workspace"]);
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "extra"]);
    cargo_in(&dir, &target.join("special"), &["build", "--workspace"]);
    let before = names(&target);
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Skipped { .. }));
    assert_eq!(before, names(&target));
}

#[test]
fn a_skipped_listing_keeps_the_stale_build_script_executable() {
    let dir = tmp("dirty-build-script");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let before = names(&target);
    std::fs::write(dir.join("build.rs"), "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n").unwrap();
    match prune(None, &target).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("not fresh"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    let after = names(&target);
    let removed: Vec<_> = before.difference(&after).collect();
    assert!(removed.is_empty(), "the skipped listing removed {removed:?}");
}

#[test]
fn listing_never_runs_a_stale_build_script() {
    let dir = tmp("no-build-script");
    let target = project(&dir, Some("sweep = [[\"build\", \"--workspace\"]]"));
    std::fs::write(dir.join("trigger"), "first").unwrap();
    std::fs::write(
        dir.join("build.rs"),
        "fn main() { println!(\"cargo:rerun-if-changed=trigger\"); std::fs::write(\"marker\", \"ran\").unwrap(); }\n",
    )
    .unwrap();
    cargo_in(&dir, &target, &["build", "--workspace"]);
    std::fs::remove_file(dir.join("marker")).unwrap();
    std::fs::write(dir.join("trigger"), "changed").unwrap();
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Skipped { .. }));
    assert!(!dir.join("marker").exists(), "the listing ran the build script");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn listing_a_test_command_does_not_run_tests() {
    let dir = tmp("no-test-run");
    let target = project(&dir, Some("sweep = [[\"test\", \"--workspace\"]]"));
    std::fs::write(dir.join("src/lib.rs"), "#[test] fn writes_marker() { std::fs::write(\"test-marker\", \"ran\").unwrap(); }\n")
        .unwrap();
    cargo_in(&dir, &target, &["test", "--workspace", "--no-run"]);
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Pruned { .. }));
    assert!(!dir.join("test-marker").exists(), "the listing ran a test");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn listing_a_doc_test_command_refuses_without_running_it() {
    let dir = tmp("no-doc-test-run");
    let target = project(&dir, Some("sweep = [[\"test\", \"--doc\"]]"));
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// ```\n/// std::fs::write(\"doc-marker\", \"ran\").unwrap();\n/// ```\npub fn f() {}\n",
    )
    .unwrap();
    cargo_in(&dir, &target, &["build"]);
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Skipped { .. }));
    assert!(!dir.join("doc-marker").exists(), "the listing ran a doc test");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_cross_target_listing_keeps_an_unlisted_host_profile() {
    let dir = tmp("partial-cross-host");
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    assert!(rustc.status.success());
    let version = String::from_utf8(rustc.stdout).unwrap();
    let triple = version.lines().find_map(|line| line.strip_prefix("host: ")).unwrap();
    let target = project(&dir, Some(&format!("sweep = [[\"build\", \"--workspace\", \"--target\", \"{triple}\"]]")));
    cargo_in(&dir, &target, &["build", "--workspace", "--target", triple]);
    let host = target.join("debug");
    std::fs::create_dir_all(host.join(".fingerprint/unlisted-0123456789abcdef")).unwrap();
    std::fs::create_dir_all(host.join("deps")).unwrap();
    let artifact = host.join("deps/libunlisted-0123456789abcdef.rlib");
    std::fs::write(&artifact, "host artifact").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 { 2 }\n").unwrap();
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Pruned { .. }));
    assert!(artifact.is_file(), "the unlisted host profile lost an artifact");
    let _ = std::fs::remove_dir_all(&dir);
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
    assert!(
        started.elapsed() < deadline - std::time::Duration::from_secs(2),
        "the sweep waited for the deadline: {:?}",
        started.elapsed()
    );
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

#[test]
fn warm_runs_every_sweep_command_when_one_fails() {
    let dir = tmp("warm-fails");
    let target = project(&dir, Some("sweep = [[\"build\", \"--features\", \"extra\"], [\"build\", \"--release\"]]"));
    // The feature build does not compile; the release build does.
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn a() -> i32 {\n    1\n}\n#[cfg(feature = \"extra\")]\ncompile_error!(\"broken\");\n",
    )
    .unwrap();
    let e = format!("{:#}", cargo::warm(&host(), &target).unwrap_err());
    assert!(e.contains("cargo build --features extra"), "{e}");
    assert!(!e.contains("--release"), "{e}");
    let stderr = cargo_in(&dir, &target, &["build", "--release"]);
    assert!(!stderr.contains("Compiling"), "the command after the failed one still warmed its profile: {stderr}");
}

#[test]
fn warm_builds_the_crates_that_do_not_need_a_broken_one() {
    // A workspace: `broken` does not compile; `fine` does not need it, but waits for `slow`, whose build script
    // runs for a second. Without --keep-going cargo starts no new job after `broken` fails, so `fine` stays cold.
    let dir = tmp("warm-keep-going");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"broken\", \"fine\", \"slow\"]\nresolver = \"2\"\n\n[workspace.metadata.layr-retest]\nsweep = [[\"build\", \"--workspace\"]]\n",
    )
    .unwrap();
    for (name, body, deps) in [
        ("broken", "compile_error!(\"broken\");\n", ""),
        ("fine", "pub fn f() { slow::s() }\n", "\n[dependencies]\nslow = { path = \"../slow\" }\n"),
        ("slow", "pub fn s() {}\n", ""),
    ] {
        std::fs::create_dir_all(dir.join(name).join("src")).unwrap();
        std::fs::write(
            dir.join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}"),
        )
        .unwrap();
        std::fs::write(dir.join(name).join("src/lib.rs"), body).unwrap();
    }
    std::fs::write(dir.join("slow/build.rs"), "fn main() { std::thread::sleep(std::time::Duration::from_secs(1)); }\n").unwrap();
    let target = dir.join("target");
    assert!(cargo::warm(&host(), &target).is_err());
    let stderr = cargo_in(&dir, &target, &["build", "-p", "fine"]);
    assert!(!stderr.contains("Compiling"), "the crate that compiles is warm: {stderr}");
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
    fn command(&self, program: &str) -> std::io::Result<Command> {
        use std::os::unix::process::CommandExt;
        let mut c = if program == "cargo" { Command::new(&self.path) } else { Command::new(program) };
        c.process_group(0);
        Ok(c)
    }

    fn deadline(&self) -> std::time::Instant {
        self.until
    }
}

#[test]
fn listing_disables_both_build_script_names_only_in_its_copy() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("no-build-script-alias");
    let target = project(&dir, Some("sweep = [[\"build\"]]"));
    let build = target.join("debug/build/demo-0123456789abcdef");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
    std::fs::write(target.join("debug/.cargo-lock"), "").unwrap();
    let aliases = [build.join("build-script-build"), build.join("build_script_build-0123456789abcdef")];
    for file in &aliases {
        std::fs::write(file, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let script = dir.join("fake-cargo.sh");
    std::fs::write(&script, "#!/bin/sh\np=\"$CARGO_TARGET_DIR/debug/build/demo-0123456789abcdef\"\nif [ -x \"$p/build-script-build\" ] || [ -x \"$p/build_script_build-0123456789abcdef\" ]; then touch unsafe-marker; fi\necho 'Dirty demo v0.1.0' >&2\nsleep 0.2\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host = Script { path: script, until: std::time::Instant::now() + std::time::Duration::from_secs(30) };
    let _ = cargo::list_units(&host, &dir, &target, &[], &["build".into()]);
    assert!(!dir.join("unsafe-marker").exists());
    for file in &aliases {
        assert_ne!(std::fs::metadata(file).unwrap().permissions().mode() & 0o111, 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_partial_listing_with_an_unrelated_error_is_not_a_live_set() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("partial-error");
    let target = project(&dir, Some("sweep = [[\"build\"]]"));
    std::fs::create_dir_all(target.join("debug/.fingerprint/demo-0123456789abcdef")).unwrap();
    std::fs::write(target.join("debug/.cargo-lock"), "").unwrap();
    std::fs::create_dir_all(target.join("debug/deps")).unwrap();
    let artifact = target.join("debug/deps/libother-0123456789abcdef.rlib");
    std::fs::write(&artifact, "keep this").unwrap();
    let script = dir.join("fake-cargo.sh");
    let metadata = serde_json::json!({"workspace_members":["demo-id"],"packages":[{"id":"demo-id","name":"demo","manifest_path":dir.join("Cargo.toml")}]}).to_string();
    let body = format!("#!/bin/sh\nif [ \"$1\" = metadata ]; then echo '{metadata}'; exit 0; fi\necho 'Dirty demo v0.1.0 ({})' >&2\nsleep 0.2\necho '{{\"reason\":\"build-finished\",\"success\":false}}'\necho LAYR_PRUNE_COMPILE_REFUSED >&2\necho 'error: could not compile `demo` (lib)' >&2\necho 'error: unrelated failure' >&2\nexit 101\n", dir.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host = Script { path: script, until: std::time::Instant::now() + std::time::Duration::from_secs(30) };
    let result = cargo::live_units(&host, &dir, &target, &[], &[vec!["build".into()]]);
    assert!(!matches!(result, Ok(cargo::Listed::Fresh(_))), "an incomplete listing cannot authorize deletion");
    assert!(matches!(cargo::prune(&host, &target), Outcome::Skipped { .. }));
    assert_eq!(std::fs::read(&artifact).unwrap(), b"keep this");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_non_workspace_package_with_the_same_name_is_not_accepted() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("same-name-external");
    let target = dir.join("target");
    let foreign = dir.join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    let manifest = dir.join("Cargo.toml");
    std::fs::write(&manifest, "[workspace]\n").unwrap();
    let metadata =
        serde_json::json!({"workspace_members":["demo-id"],"packages":[{"id":"demo-id","name":"demo","manifest_path":manifest}]})
            .to_string();
    let script = dir.join("fake-cargo.sh");
    let body = format!("#!/bin/sh\nif [ \"$1\" = metadata ]; then echo '{metadata}'; exit 0; fi\necho 'Dirty demo v0.1.0 ({})' >&2\nsleep 0.2\necho '{{\"reason\":\"build-finished\",\"success\":false}}'\necho LAYR_PRUNE_COMPILE_REFUSED >&2\necho 'error: could not compile `demo` (lib)' >&2\nexit 101\n", foreign.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host = Script { path: script, until: std::time::Instant::now() + std::time::Duration::from_secs(30) };
    let result = cargo::live_units(&host, &dir, &target, &[], &[vec!["build".into()]]);
    assert!(!matches!(result, Ok(cargo::Listed::Fresh(_))), "a different package is not a workspace member");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_partial_listing_with_invalid_json_cannot_authorize_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp("partial-invalid-json");
    let target = dir.join("target");
    let manifest = dir.join("Cargo.toml");
    std::fs::write(&manifest, "[workspace]\n").unwrap();
    let metadata =
        serde_json::json!({"workspace_members":["demo-id"],"packages":[{"id":"demo-id","name":"demo","manifest_path":manifest}]})
            .to_string();
    let script = dir.join("fake-cargo.sh");
    let body = format!("#!/bin/sh\nif [ \"$1\" = metadata ]; then echo '{metadata}'; exit 0; fi\necho 'Dirty demo v0.1.0 ({})' >&2\nsleep 0.2\necho '{{broken'\necho '{{\"reason\":\"build-finished\",\"success\":false}}'\necho LAYR_PRUNE_COMPILE_REFUSED >&2\necho 'error: could not compile `demo` (lib)' >&2\nexit 101\n", dir.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host = Script { path: script, until: std::time::Instant::now() + std::time::Duration::from_secs(30) };
    let result = cargo::live_units(&host, &dir, &target, &[], &[vec!["build".into()]]);
    assert!(!matches!(result, Ok(cargo::Listed::Fresh(_))), "invalid JSON cannot authorize deletion");
    let _ = std::fs::remove_dir_all(&dir);
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
    let env =
        vec![("CARGO_HOME".to_string(), home.display().to_string()), ("DAEMON_PID".to_string(), daemon.display().to_string())];
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
    for d in [
        "debug/deps",
        "release/deps",
        "retest/debug",
        "profraw/other",
        "x86_64-unknown-linux-gnu/debug",
        "doc/prunews",
        "custom/deps",
        "aarch64-apple-darwin/debug",
    ] {
        std::fs::create_dir_all(target.join(d)).unwrap();
        std::fs::write(target.join(d).join("f"), "x").unwrap();
    }
    std::fs::write(target.join("CACHEDIR.TAG"), "x").unwrap();
    let commands: Vec<Vec<String>> =
        [vec!["build"], vec!["build", "--release"], vec!["build", "--target", "aarch64-apple-darwin"]]
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

/// A project whose crate `prunews` uses the crate `dep` by path from out of the workspace, so `dep` is not a
/// workspace member, like a registry crate. `dep` has the feature `extra`. Returns (project, its target folder).
fn project_with_dep(base: &Path, sweep: &str) -> (PathBuf, PathBuf) {
    let dep = base.join("dep");
    std::fs::create_dir_all(dep.join("src")).unwrap();
    std::fs::write(
        dep.join("Cargo.toml"),
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n",
    )
    .unwrap();
    std::fs::write(dep.join("src/lib.rs"), "pub fn d() -> i32 {\n    1\n}\n").unwrap();
    let dir = base.join("project");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"prunews\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = {{ path = \"../dep\" }}\n\n[workspace]\n\n[workspace.metadata.layr-retest]\n{sweep}\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 {\n    dep::d()\n}\n").unwrap();
    let target = dir.join("target");
    (dir, target)
}

/// The `.fingerprint` folders of the crate `name` in `target/debug`.
fn units_of(target: &Path, name: &str) -> BTreeSet<String> {
    std::fs::read_dir(target.join("debug/.fingerprint"))
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.rsplit_once('-').is_some_and(|(p, _)| p == name))
        .collect()
}

#[test]
fn a_stale_clippy_after_a_source_edit_still_sweeps_and_its_next_run_keeps_the_dependencies() {
    let base = tmp("stale-clippy");
    let (dir, target) = project_with_dep(&base, "sweep = [[\"build\", \"--workspace\"], [\"clippy\", \"--workspace\"]]");
    cargo_in(&dir, &target, &["build", "--workspace"]);
    cargo_in(&dir, &target, &["clippy", "--workspace"]);
    let standard_dep = units_of(&target, "dep");
    // Garbage: a build with a feature of `dep` makes units with other hashes.
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "dep/extra"]);
    let garbage: BTreeSet<String> = units_of(&target, "dep").difference(&standard_dep).cloned().collect();
    assert!(!garbage.is_empty());
    // A source edit and a build: the clippy units of the workspace crate are stale now, as in a line whose gate
    // runs no clippy.
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() -> i32 {\n    dep::d() + 1\n}\n").unwrap();
    cargo_in(&dir, &target, &["build", "--workspace"]);

    match prune(None, &target).unwrap().1 {
        Outcome::Pruned { freed } => assert!(freed > 0),
        other => panic!("expected a prune, got {other:?}"),
    }
    let left = units_of(&target, "dep");
    assert!(left.is_disjoint(&garbage), "the feature units of dep stay: {left:?}");
    assert_eq!(left, standard_dep);
    // The next clippy checks the edited crate only: the clippy unit of `dep` is still warm.
    let stderr = cargo_in(&dir, &target, &["clippy", "--workspace", "-v"]);
    assert!(stderr.contains("Checking prunews") && !stderr.contains("Checking dep"), "{stderr}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_stale_unit_out_of_the_workspace_still_skips_the_folder() {
    let base = tmp("stale-dep");
    let (dir, target) = project_with_dep(&base, "sweep = [[\"build\", \"--workspace\"], [\"clippy\", \"--workspace\"]]");
    cargo_in(&dir, &target, &["build", "--workspace"]);
    cargo_in(&dir, &target, &["clippy", "--workspace"]);
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "dep/extra"]);
    let before = units_of(&target, "dep");
    // `dep` changes and only the build runs: its clippy unit is stale, and `dep` is not a workspace member.
    std::fs::write(base.join("dep/src/lib.rs"), "pub fn d() -> i32 {\n    2\n}\n").unwrap();
    cargo_in(&dir, &target, &["build", "--workspace"]);

    match prune(None, &target).unwrap().1 {
        Outcome::Skipped { reason } => {
            assert!(reason.contains("not fresh") && reason.contains("dep is not a workspace member"), "{reason}")
        }
        other => panic!("expected a skip, got {other:?}"),
    }
    assert_eq!(units_of(&target, "dep"), before);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_stale_workspace_dependency_keeps_the_other_dependencies_of_its_dependents() {
    let base = tmp("stale-workspace-graph");
    let dep = base.join("dep");
    std::fs::create_dir_all(dep.join("src")).unwrap();
    std::fs::write(dep.join("Cargo.toml"), "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n[features]\nextra=[]\n")
        .unwrap();
    std::fs::write(dep.join("src/lib.rs"), "pub fn d() -> i32 { 1 }\n").unwrap();
    let dir = base.join("project");
    for name in ["basecrate", "consumer"] {
        std::fs::create_dir_all(dir.join(name).join("src")).unwrap();
    }
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers=['basecrate','consumer']\nresolver='2'\n[workspace.metadata.layr-retest]\nsweep=[[\"build\",\"--workspace\"]]\n").unwrap();
    std::fs::write(dir.join("basecrate/Cargo.toml"), "[package]\nname='basecrate'\nversion='0.1.0'\nedition='2021'\n").unwrap();
    std::fs::write(dir.join("basecrate/src/lib.rs"), "pub fn value() -> i32 { 1 }\n").unwrap();
    std::fs::write(dir.join("consumer/Cargo.toml"), "[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nbasecrate={path='../basecrate'}\ndep={path='../../dep'}\n").unwrap();
    std::fs::write(dir.join("consumer/src/lib.rs"), "pub fn value() -> i32 { basecrate::value() + dep::d() }\n").unwrap();
    let target = dir.join("target");
    cargo_in(&dir, &target, &["build", "--workspace"]);
    let needed = units_of(&target, "dep");
    cargo_in(&dir, &target, &["build", "--workspace", "--features", "dep/extra"]);
    let garbage: BTreeSet<String> = units_of(&target, "dep").difference(&needed).cloned().collect();
    assert!(!garbage.is_empty());
    std::fs::write(dir.join("basecrate/src/lib.rs"), "pub fn value() -> i32 { 2 }\n").unwrap();
    assert!(matches!(prune(None, &target).unwrap().1, Outcome::Pruned { .. }));
    let left = units_of(&target, "dep");
    assert!(needed.is_subset(&left) && left.is_disjoint(&garbage), "needed={needed:?} left={left:?}");
    let stderr = cargo_in(&dir, &target, &["build", "--workspace", "-v"]);
    assert!(!stderr.contains("Compiling dep"), "{stderr}");
    let _ = std::fs::remove_dir_all(&base);
}
