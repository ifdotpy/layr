//! End to end: a tiny cargo project in a layr line with two tests on two functions.
//! Runs `layr retest ...`. Needs the `layr` built from this repository first in PATH
//! (`cargo build -p layr`, then `target/debug` first in PATH),
//! `cargo nextest`, the registry package `hex` 0.4 in the cargo cache (the test runs offline), `cargo nextest` and the `llvm-tools` rustup component. The store is a temp
//! folder with its own LAYR_ROOT, and every test-launched `layr` command runs with LAYR_DIRECT=1: without it a
//! client of a normal user forwards to the system daemon, which uses its own store. Takes about 10 s.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The time limit of one test process. A cold cargo build of the test project fits in it.
const LIMIT: Duration = Duration::from_secs(1800);

/// Waits for `child` for at most `limit`. The child must start its own process group (see `group`). At the
/// limit the group is killed, so no child lives on after the test, and the test fails.
fn finish(child: std::process::Child, limit: Duration) -> std::process::Output {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(out) => out.unwrap(),
        Err(_) => {
            let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
            panic!("a test process ran past {limit:?}; its process group was killed");
        }
    }
}

/// Runs `cmd` in its own process group with the time limit of `finish`. Its stdin is empty.
fn run_limited(cmd: &mut Command) -> std::process::Output {
    let child = cmd.process_group(0).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    finish(child, LIMIT)
}

/// `output()` of a command, limited like `run_limited`.
trait OutputLimited {
    fn output_limited(&mut self) -> std::process::Output;
}

impl OutputLimited for Command {
    fn output_limited(&mut self) -> std::process::Output {
        run_limited(self)
    }
}

// `tc` reads a file, which coverage cannot track.
const LIB: &str = "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn ta() {\n        assert_eq!(a(), 1);\n    }\n\n    #[test]\n    fn tb() {\n        assert_eq!(b(), 2);\n    }\n\n    #[test]\n    fn th() {\n        assert_eq!(hex::FromHexError::OddLength.to_string(), \"Odd number of digits\");\n    }\n\n    #[test]\n    fn tc() {\n        assert_eq!(std::fs::read_to_string(\"data.txt\").unwrap(), \"ok\");\n    }\n}\n";

const KID: &str = "fn work() -> i32 {\n    7\n}\n\nfn main() {\n    print!(\"{}\", work());\n}\n";
const KID_TEST: &str = "#[test]\nfn tk() {\n    let o = std::process::Command::new(env!(\"CARGO_BIN_EXE_kid\")).output().unwrap();\n    assert_eq!(o.stdout, b\"7\");\n    // A child with a cleared environment writes a default profile into the cwd.\n    std::process::Command::new(env!(\"CARGO_BIN_EXE_kid\")).env_clear().output().unwrap();\n}\n";

// A bin that a test stops with SIGKILL: LLVM writes no profile, so its code is in no record.
const HANG: &str = "fn work() -> i32 {\n    1\n}\n\nfn main() {\n    println!(\"up {}\", work());\n    loop {\n        std::thread::sleep(std::time::Duration::from_secs(1));\n    }\n}\n";
const HANG_TEST: &str = "use std::io::BufRead;\n\n#[test]\nfn tkill() {\n    let mut c = std::process::Command::new(env!(\"CARGO_BIN_EXE_hang\")).stdout(std::process::Stdio::piped()).spawn().unwrap();\n    let mut line = String::new();\n    std::io::BufReader::new(c.stdout.take().unwrap()).read_line(&mut line).unwrap();\n    assert!(line.starts_with(\"up\"));\n    c.kill().unwrap();\n    c.wait().unwrap();\n}\n";

fn sh(dir: &Path, root: &Path, prog: &str, args: &[&str]) -> (String, String) {
    let o = run_limited(
        Command::new(prog)
            .args(args)
            .current_dir(dir)
            .env("LAYR_ROOT", root).env("LAYR_DIRECT", "1")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR")
            .env("CARGO_NET_OFFLINE", "true")
            // The assertions match plain text. CI sets CARGO_TERM_COLOR=always, which puts escape codes inside test names.
            .env("CARGO_TERM_COLOR", "never"),
    );
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{prog} {args:?} failed:\n{out}\n{err}");
    (out, err)
}

/// The newest record file of a line (`<state>.json`; the folder is a cache, so layr keeps other files there too,
/// such as `.layr-times`).
fn last_record_path(line: &Path) -> PathBuf {
    std::fs::read_dir(line.join(".layr-retest"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .max_by_key(|p| p.metadata().unwrap().modified().unwrap())
        .unwrap()
}

fn last_record(line: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(last_record_path(line)).unwrap()).unwrap()
}

/// Files of the line folder, without `target/`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() && p.file_name().is_some_and(|n| n != "target") {
            out.extend(walk(&p));
        } else if p.is_file() {
            out.push(p);
        }
    }
    out
}

fn retest(line: &Path, root: &Path, args: &[&str]) -> String {
    let t = std::time::Instant::now();
    let mut all = vec!["retest"];
    all.extend_from_slice(args);
    let (_, err) = sh(line, root, "layr", &all);
    eprintln!("layr retest {args:?}: {:.1}s\n{err}", t.elapsed().as_secs_f32());
    err
}

#[test]
fn runs_only_the_stale_test() {
    // layr refuses a store path with a symlink in it (/var is one on macOS).
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-e2e-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(
        src.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [[\"test\", \"--workspace\", \"--no-run\"]]\n\n[dependencies]\nhex = \"0.4\"\n",
    )
    .unwrap();
    std::fs::write(src.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(src.join("data.txt"), "ok").unwrap();
    // `tk` runs a binary with the inherited environment; `work` runs only in that child.
    std::fs::create_dir_all(src.join("src/bin")).unwrap();
    std::fs::create_dir_all(src.join("tests")).unwrap();
    std::fs::write(src.join("src/bin/kid.rs"), KID).unwrap();
    std::fs::write(src.join("tests/kid.rs"), KID_TEST).unwrap();
    std::fs::write(src.join("src/bin/hang.rs"), HANG).unwrap();
    std::fs::write(src.join("tests/kill.rs"), HANG_TEST).unwrap();
    // `target` is a second cache path: the run must save only the records, not the build output.
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\ntarget regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    // The lock file is part of the project before the first run, as in a real project.
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");

    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    let main: PathBuf = root.join("projects/app/lines/main");
    sh(&main, &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    let lib = line.join("src/lib.rs");

    // 1. No record: every test runs. `--save` puts the records in the cache overlay.
    let err = retest(&line, &root, &["run", "--save"]);
    assert!(err.contains("6 tests, 6 selected"), "{err}");

    assert!(err.contains("removed 1 stray .profraw"), "{err}");

    // Processes outside a test (the list phase, build scripts) leave no profile in the project.
    let stray: Vec<_> = walk(&line).into_iter().filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    assert!(stray.is_empty(), "{stray:?}");

    // Only the records went to a cache overlay.
    let (overlays, _) = sh(&line, &root, "layr", &["overlay", "list"]);
    assert!(overlays.contains("folder=.layr-retest") && !overlays.contains("folder=target"), "{overlays}");

    // The test that uses `hex` has the registry package in its record; the others do not.
    let rec: serde_json::Value = last_record(&line);
    let deps = |t: &str| rec["tests"][format!("tiny {t}")]["deps"].to_string();
    assert!(deps("tests::th").contains("hex-0.4."), "{rec}");
    assert_eq!(deps("tests::ta"), "[]", "{rec}");

    // 1b. Doctor: the two integration tests are two binaries, which it says; it reads and writes nothing.
    let before = walk(&line).len();
    let o = Command::new("layr").args(["retest", "doctor"]).current_dir(&line).env("LAYR_ROOT", &root).env("LAYR_DIRECT", "1").output_limited();
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(text.contains("tiny: 2 integration test binaries"), "{text}");
    assert!(text.contains("ok: 2 records") || text.contains("ok: 1 records"), "{text}");
    assert_eq!(walk(&line).len(), before);

    // 2. No change: nothing runs.
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected") && err.contains("no build"), "{err}");

    // 2b. A comment above everything shifts the lines. It cannot add a test: no build, and the
    // records move. A run with other nextest arguments cannot use them and builds.
    std::fs::write(&lib, format!("// note\n{LIB}")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected") && err.contains("no build"), "{err}");
    let err = retest(&line, &root, &["run", "--", "--workspace"]);
    assert!(err.contains("6 tests, 0 selected") && !err.contains("no build"), "{err}");
    std::fs::write(&lib, LIB).unwrap();
    let err = retest(&line, &root, &["run", "--", "--workspace"]);
    assert!(err.contains("no build"), "{err}");
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected") && !err.contains("no build"), "{err}");

    // 3. Change the body of `a`: only `ta` runs. The build leaves old files behind: the sweep
    // removes what this build did not use and keeps the rest (the run below needs the test binaries).
    let debug = line.join("target/retest/debug");
    let (zombie, zombie_meta) = (debug.join("deps/libzombie-0123456789abcdef.rlib"), debug.join("deps/zombie-0123456789abcdef.d"));
    std::fs::write(&zombie, vec![0u8; 3_000_000]).unwrap();
    std::fs::write(&zombie_meta, "x").unwrap();
    let inc = std::fs::read_dir(debug.join("incremental")).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("tiny-")).unwrap();
    std::fs::create_dir_all(inc.join("s-0-old/x")).unwrap();
    std::fs::write(inc.join("s-0-old.lock"), "").unwrap();
    // Make the old session the oldest.
    std::fs::File::open(inc.join("s-0-old")).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
    std::fs::write(&lib, LIB.replace("    1\n", "    1 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 1 selected"), "{err}");
    assert!(err.contains("ran 1, passed 1"), "{err}");
    assert!(err.contains("swept stale build output, freed "), "{err}");
    assert!(!zombie.exists() && !zombie_meta.exists() && !inc.join("s-0-old").exists() && !inc.join("s-0-old.lock").exists());
    let live = std::fs::read_dir(debug.join("deps")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("libtiny-")).count();
    assert!(live >= 1, "the sweep removed the live rlib of the package");
    assert!(std::fs::read_dir(&inc).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("s-")), "the newest session stays");

    // 4. A new function that no test calls: nothing runs.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("pub fn b()", "pub fn c() -> i32 {\n    3\n}\n\npub fn b()")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected"), "{err}");

    // 4b. An uncalled function above `a` shifts every line: nothing runs, the records must shift.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("pub fn d() -> i32 {{\n    4\n}}\n\n{s}")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected"), "{err}");

    // 4c. Edit `a` at its new place: `ta` must run, `tb` must not.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("    1 + 0\n", "    1 + 0 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 1 selected"), "{err}");
    assert!(err.contains("tiny tests::ta") && !err.contains("tiny tests::tb"), "{err}");

    // 4d. Code that runs only in a child process: editing it selects the test that started it.
    let kid = line.join("src/bin/kid.rs");
    std::fs::write(&kid, KID.replace("    7\n", "    7 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 1 selected"), "{err}");
    assert!(err.contains("tiny::kid tk") && err.contains("ran 1, passed 1"), "{err}");

    // 4e. Code that runs only in a child that the test kills: no profile, so no record has the file.
    // A change of it must run every test, and say why.
    let hang = line.join("src/bin/hang.rs");
    assert!(!std::fs::read_to_string(last_record_path(&line)).unwrap().contains("src/bin/hang.rs"));
    std::fs::write(&hang, HANG.replace("    1\n", "    1 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 6 selected") && err.contains("src/bin/hang.rs"), "{err}");
    assert!(err.contains("ran 6, passed 6"), "{err}");

    // 5. A file that coverage cannot see changes: nothing runs, and nothing is built.
    std::fs::write(line.join("data.txt"), "bad").unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected") && err.contains("no build"), "{err}");

    // 6. A full run runs all, fails `tc`, and reports the contradiction with the saved pass.
    let o = Command::new("layr")
        .args(["retest", "run", "--full"])
        .current_dir(&line)
        .env("LAYR_ROOT", &root).env("LAYR_DIRECT", "1")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .output_limited();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("6 selected") && err.contains("CONTRADICTION: tiny tests::tc"), "{err}");

    // 7. The records are in the cache overlay: a line made from this line takes them.
    std::fs::write(line.join("data.txt"), "ok").unwrap();
    sh(&line, &root, "layr", &["save", "-a", "-m", "work"]);
    retest(&line, &root, &["run", "--save"]);
    sh(&main, &root, "layr", &["line", "new", "work2", "work"]);
    let err = retest(&root.join("projects/app/lines/work2"), &root, &["run"]);
    assert!(err.contains("6 tests, 0 selected"), "{err}");

    // 8. A new test in a file that no recorded test selects: it is found and runs, no false pass.
    let lib2 = root.join("projects/app/lines/work2/src/lib.rs");
    let s = std::fs::read_to_string(&lib2).unwrap();
    std::fs::write(&lib2, s.replace("    #[test]\n    fn tb()", "    #[test]\n    fn tn() {\n        assert_eq!(b(), 2);\n    }\n\n    #[test]\n    fn tb()")).unwrap();
    let err = retest(&root.join("projects/app/lines/work2"), &root, &["run"]);
    assert!(err.contains("7 tests, 1 selected") && err.contains("tiny tests::tn") && !err.contains("no build"), "{err}");

    // 9. Without --save the run writes the record file and leaves the overlay alone.
    std::fs::write(&lib2, std::fs::read_to_string(&lib2).unwrap().replace("    2\n", "    2 + 0\n")).unwrap();
    let err = retest(&root.join("projects/app/lines/work2"), &root, &["run"]);
    assert!(err.contains("7 tests, 2 selected") && err.contains("ran 2, passed 2"), "{err}");

    // Saved states are immutable on macOS; unlock them before the delete.
    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// vitest adapter with a fake tool: a script that logs its arguments and exits with the code in a file.
/// No real vitest, jest or pytest-testmon is used.
#[test]
fn vitest_adapter_with_a_fake_tool() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-tool-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.ts"), "export const a = 1;\n").unwrap();
    std::fs::write(src.join("b.ts"), "export const b = 2;\n").unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/.layr-retest\n").unwrap();
    let (log, code) = (tmp.join("argv.log"), tmp.join("code"));
    let fake = tmp.join("fake-vitest");
    std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >> {}\nexit $(cat {})\n", log.display(), code.display())).unwrap();
    std::fs::write(&code, "0").unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/app/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");

    let run = |args: &[&str]| -> (bool, String) {
        let o = Command::new("layr")
            .arg("retest")
            .args(args)
            .current_dir(&line)
            .env("LAYR_ROOT", &root).env("LAYR_DIRECT", "1")
            .env("LAYR_RETEST_VITEST", &fake)
            .output_limited();
        let _ = std::fs::read_to_string(&log); // the log is read by `calls`
        (o.status.success(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let calls = || -> Vec<String> { std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect() };

    // 1. No baseline: the whole suite.
    assert!(run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls(), vec!["run"]);

    // 2. No change: the tool does not run, and the run passes.
    let (ok, err) = run(&["run", "--tool", "vitest"]);
    assert!(ok && err.contains("nothing to run"), "{err}");
    assert_eq!(calls().len(), 1);

    // 3. One file changed: only `related --run` that file.
    std::fs::write(line.join("a.ts"), "export const a = 10;\n").unwrap();
    assert!(run(&["run", "--tool", "vitest", "--", "--bail"]).0);
    assert_eq!(calls().last().unwrap(), "related --run a.ts --bail");

    // 4. A failing run writes no pass record: the next run starts from the same baseline again.
    std::fs::write(&code, "1").unwrap();
    std::fs::write(line.join("b.ts"), "export const b = 20;\n").unwrap();
    assert!(!run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls().last().unwrap(), "related --run b.ts");
    std::fs::write(&code, "0").unwrap();
    assert!(run(&["run", "--tool", "vitest"]).0);
    assert_eq!(calls().last().unwrap(), "related --run b.ts");
    assert_eq!(calls().len(), 4);

    // 5. After a pass nothing is left; `--full` runs everything.
    let (ok, err) = run(&["run", "--tool", "vitest"]);
    assert!(ok && err.contains("nothing to run"), "{err}");
    assert!(run(&["run", "--tool", "vitest", "--full"]).0);
    assert_eq!(calls().last().unwrap(), "run");

    // The records of the tool are apart from the nextest records.
    assert!(line.join(".layr-retest/vitest").is_dir());

    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A workspace dylib holds the code of a dependency (here `Display` of a `hex` error, which is not
/// generic). The record of a test that runs it must list the package, so the dylib reaches `llvm-cov export` as an object.
#[test]
fn workspace_dylib_is_an_export_object() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-dyn-{}", std::process::id()));
    let src = tmp.join("src-project");
    for d in ["src", "dyn-deps/src"] {
        std::fs::create_dir_all(src.join(d)).unwrap();
    }
    std::fs::write(
        src.join("Cargo.toml"),
        "[package]\nname = \"dyn-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"dyn-deps\"]\n\n[dev-dependencies]\ndyn-deps = { path = \"dyn-deps\" }\nhex = \"0.4\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("dyn-deps/Cargo.toml"),
        "[package]\nname = \"dyn-deps\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"dylib\"]\n\n[dependencies]\nhex = \"0.4\"\n",
    )
    .unwrap();
    std::fs::write(src.join("dyn-deps/src/lib.rs"), "extern crate hex;\n").unwrap();
    std::fs::write(
        src.join("src/lib.rs"),
        "#[cfg(test)]\nextern crate dyn_deps;\n\npub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn th() {\n        assert_eq!(hex::FromHexError::OddLength.to_string(), \"Odd number of digits\");\n    }\n\n    #[test]\n    fn ta() {\n        assert_eq!(super::a(), 1);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(src.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\ntarget regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "dynapp", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/dynapp/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/dynapp/lines/work");

    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("2 tests, 2 selected") && err.contains("failed 0"), "{err}");
    // The test binary of `main.rs` does not use the dylib. Built instrumented, it must link std
    // like the plain build, so that a copy of it runs elsewhere (a dylib itself needs `@rpath/libstd`).
    let deps_dir = line.join("target/retest/debug/deps");
    let linkage = |p: &std::path::Path| {
        let p = p.to_str().unwrap();
        if cfg!(target_os = "macos") { sh_out("otool", &["-L", p]) } else { sh_out("ldd", &[p]) }
    };
    let plain: Vec<String> = std::fs::read_dir(&deps_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("dyn_app-") && p.extension().is_none())
        .map(|p| linkage(&p))
        .filter(|l| !l.contains("dyn_deps"))
        .collect();
    assert_eq!(plain.len(), 1, "expected one binary without the dylib: {plain:?}");
    assert!(!plain[0].contains("libstd-"), "instrumented binary links std dynamically: {}", plain[0]);
    let rec = last_record(&line);
    assert!(rec["tests"]["dyn-app tests::th"]["deps"].to_string().contains("hex-0.4."), "{rec}");
    assert_eq!(rec["tests"]["dyn-app tests::ta"]["deps"].to_string(), "[]", "{rec}");
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("2 tests, 0 selected"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// `layr retest init` edits `.layrattributes` and `.gitignore` once, and a second run changes nothing.
#[test]
fn init_is_idempotent() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let src = tmp.join("src-project");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(src.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(src.join("src/lib.rs"), "").unwrap();
    std::fs::write(src.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/node_modules\n").unwrap();
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/app/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    // The cache folders exist in the line, as after a build and an install.
    std::fs::create_dir_all(line.join("target/debug")).unwrap();
    std::fs::write(line.join("target/debug/marker"), "x").unwrap();
    std::fs::create_dir_all(line.join("node_modules/pkg")).unwrap();
    std::fs::write(line.join("node_modules/pkg/index.js"), "x").unwrap();
    let init = || {
        let o = Command::new("layr").args(["retest", "init"]).current_dir(&line).env("LAYR_ROOT", &root).env("LAYR_DIRECT", "1").output_limited();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let overlay_folders = || {
        let (out, _) = sh(&line, &root, "layr", &["overlay", "list", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut f: Vec<String> = v.as_array().unwrap().iter().map(|o| o["folder"].as_str().unwrap().to_string()).collect();
        f.sort();
        f
    };
    let files = || {
        let read = |p: &str| std::fs::read_to_string(line.join(p)).unwrap();
        (read(".layrattributes"), read("Cargo.toml"), read(".gitignore"))
    };

    let (code, out) = init();
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("layr retest gate -- <your gate command>"), "{out}");
    assert!(out.contains("target: saved as cache overlay") && out.contains("node_modules: saved as cache overlay"), "{out}");
    assert_eq!(overlay_folders(), ["node_modules", "target"]);
    let (attrs, manifest, ignore) = files();
    assert_eq!(attrs, "target regenerable\nnode_modules regenerable\n.layr-retest regenerable\n");
    assert_eq!(ignore, "/target\n/node_modules\n.layr-retest\n");
    assert!(
        manifest.contains("[workspace.metadata.layr-retest]") && manifest.contains("CARGO_INCREMENTAL = \"1\""),
        "{manifest}"
    );
    assert!(
        manifest.contains("sweep = [[\"build\", \"--locked\"], [\"test\", \"--locked\", \"--no-run\"], [\"build\", \"--release\", \"--locked\"]]"),
        "{manifest}"
    );

    // Second run: nothing changes, no cache is saved again.
    let (code, out) = init();
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("target: already a cache overlay") && !out.contains("saved as cache overlay"), "{out}");
    assert_eq!(overlay_folders(), ["node_modules", "target"]);
    assert_eq!(files(), (attrs, manifest, ignore));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A project in a layr line. `files` are written to the source, which becomes the `main` line of the project.
fn new_line(name: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf, PathBuf) {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let src = tmp.join("src-project");
    for (path, text) in files {
        let p = src.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\ntarget regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/app/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    (tmp, root, line)
}

/// A build script that sets the cfg `e2e_flag` when `LAYR_E2E_FLAG` is in the build environment.
const ENV_BUILD: &str = "fn main() {\n    println!(\"cargo:rerun-if-env-changed=LAYR_E2E_FLAG\");\n    println!(\"cargo:rustc-check-cfg=cfg(e2e_flag)\");\n    if std::env::var_os(\"LAYR_E2E_FLAG\").is_some() {\n        println!(\"cargo:rustc-cfg=e2e_flag\");\n    }\n}\n";
/// A library that does not build without the cfg of `ENV_BUILD`.
const ENV_LIB: &str = "#[cfg(not(e2e_flag))]\ncompile_error!(\"LAYR_E2E_FLAG did not reach the build\");\n\npub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn ta() {\n        assert_eq!(super::a(), 1);\n    }\n}\n";
/// The project env sets `LAYR_E2E_FLAG`, the one variable that `ENV_BUILD` needs.
const ENV_TOML: &str = "[package]\nname = \"envp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [[\"test\", \"--workspace\", \"--no-run\"]]\nenv = { LAYR_E2E_FLAG = \"1\" }\n";

#[test]
fn run_applies_the_project_env() {
    // Without the env of `[workspace.metadata.layr-retest]`, the build fails and `retest` exits with an error.
    let (tmp, root, line) = new_line("run-env", &[("Cargo.toml", ENV_TOML), ("build.rs", ENV_BUILD), ("src/lib.rs", ENV_LIB)]);
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("passed 1, failed 0"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn sweep_applies_the_project_env() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let dir = tmp.join("project");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), ENV_TOML).unwrap();
    std::fs::write(dir.join("build.rs"), ENV_BUILD).unwrap();
    std::fs::write(dir.join("src/lib.rs"), ENV_LIB).unwrap();
    cargo(&dir, &["generate-lockfile"]);
    // The standard build runs with the project env. The sweep must list the same units, or it skips the folder.
    cargo_in(&dir, &dir.join("target"), &[("LAYR_E2E_FLAG", "1")], &["test", "--workspace", "--no-run"]);
    let err = sweep_cmd(&dir, &[]);
    assert!(err.contains("swept target"), "{err}");
    assert!(!err.contains("sweep skipped"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// `layr retest gate` in `dir`, with the lock at `lock` and no load wait.
fn gate_child(dir: &Path, lock: &Path, args: &[&str]) -> std::process::Child {
    let mut all = vec!["retest", "gate", "--max-load", "100000"];
    all.extend_from_slice(args);
    Command::new("layr")
        .args(all)
        .current_dir(dir)
        .env("LAYR_ROOT", dir.join("unused-store")).env("LAYR_DIRECT", "1")
        .env("LAYR_RETEST_GATE_LOCK", lock)
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn gate_runs_one_gate_at_a_time() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-gate-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let lock = tmp.join("gate.lock");
    let marker = tmp.join("first-done");
    let first =
        gate_child(&tmp, &lock, &["--name", "first", "--", "sh", "-c", &format!("sleep 3 && touch '{}'", marker.display())]);
    let t = std::time::Instant::now();
    while !lock.join("pid").is_file() {
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "the first gate took no lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // The second gate's command passes only when the first gate's command has finished.
    let second = gate_child(&tmp, &lock, &["--name", "second", "--", "sh", "-c", &format!("test -f '{}'", marker.display())]);
    let second = finish(second, LIMIT);
    let first = finish(first, LIMIT);
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("waiting for the gate lock") && err.contains("first"), "{err}");
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(second.status.success(), "{err}");
    assert!(!lock.exists(), "the lock is released at the end");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn gate_reaps_the_lock_of_a_dead_process() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-gate-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let lock = tmp.join("gate.lock");
    let mut dead = Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), pid.to_string()).unwrap();
    std::fs::write(lock.join("owner"), "crashed-gate").unwrap();
    let out = finish(gate_child(&tmp, &lock, &["--", "true"]), LIMIT);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("reaped") && err.contains("crashed-gate"), "{err}");
    assert!(!lock.exists());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn gate_applies_the_project_env_and_returns_the_command_status() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-gate-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let good = tmp.join("good");
    std::fs::create_dir_all(good.join("src")).unwrap();
    std::fs::write(good.join("Cargo.toml"), ENV_TOML).unwrap();
    std::fs::write(good.join("build.rs"), ENV_BUILD).unwrap();
    std::fs::write(good.join("src/lib.rs"), ENV_LIB).unwrap();
    cargo(&good, &["generate-lockfile"]);
    let lock = tmp.join("gate.lock");
    // The command sees the project env, no CARGO_TARGET_DIR and a cleared RUSTC_WRAPPER. Its status is 7.
    let check = "test \"$LAYR_E2E_FLAG\" = 1 && test -z \"${CARGO_TARGET_DIR+x}\" && test \"${RUSTC_WRAPPER-unset}\" = \"\" && exit 7; exit 1";
    let out = finish(gate_child(&good, &lock, &["--", "sh", "-c", check]), LIMIT);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(7), "{err}");
    assert!(err.contains("sweep ok"), "{err}");
    assert!(!lock.exists());

    // A failing sweep is logged and does not change the status of the command.
    let bad = tmp.join("bad");
    std::fs::create_dir_all(bad.join("src")).unwrap();
    std::fs::write(
        bad.join("Cargo.toml"),
        ENV_TOML.replace("sweep = [[\"test\", \"--workspace\", \"--no-run\"]]", "sweep = [[\"no-such-cargo-command\"]]"),
    )
    .unwrap();
    std::fs::write(bad.join("src/lib.rs"), "").unwrap();
    cargo(&bad, &["generate-lockfile"]);
    // A folder that a build wrote: it has a profile folder with a fingerprint folder.
    std::fs::create_dir_all(bad.join("target/debug/.fingerprint")).unwrap();
    let out = finish(gate_child(&bad, &lock, &["--", "true"]), LIMIT);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("sweep failed"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Stdout of a tool run, for linkage checks.
fn sh_out(prog: &str, args: &[&str]) -> String {
    let o = run_limited(Command::new(prog).args(args));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

const FLAG_LIB: &str = "#[cfg(not(projflag))]\ncompile_error!(\"projflag is missing: the project rustflags were not passed\");\n\npub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn ta() {\n        assert_eq!(super::a(), 1);\n    }\n}\n";

/// Runs `layr retest run` on a project whose only `projflag` comes from its cargo config.
/// Without the project flags the instrumented build fails to compile.
fn project_flags_run(name: &str, config: &str) {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-{name}-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::create_dir_all(src.join(".cargo")).unwrap();
    std::fs::write(src.join("Cargo.toml"), "[package]\nname = \"flags\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(src.join(".cargo/config.toml"), config).unwrap();
    std::fs::write(src.join("src/lib.rs"), FLAG_LIB).unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\ntarget regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    let main: PathBuf = root.join("projects/app/lines/main");
    sh(&main, &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("passed 1, failed 0"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn build_rustflags_reach_the_instrumented_build() {
    project_flags_run("build-flags", "[build]\nrustflags = [\"--cfg\", \"projflag\"]\n");
}

#[test]
fn target_rustflags_reach_the_instrumented_build() {
    let host = sh_out("rustc", &["-vV"]);
    let host = host.lines().find_map(|l| l.strip_prefix("host: ")).expect("rustc host").to_string();
    project_flags_run("target-flags", &format!("[target.{host}]\nrustflags = [\"--cfg\", \"projflag\"]\n"));
}

/// A workspace of two crates for the sweep: `lib-a` (feature `extra`) and `app` (feature `app-only`).
/// `sweep` is the `sweep = [...]` line of the workspace metadata.
fn sweep_workspace(dir: &Path, sweep: &str) {
    std::fs::create_dir_all(dir.join("lib-a/src")).unwrap();
    std::fs::create_dir_all(dir.join("app/src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"lib-a\", \"app\"]\nresolver = \"2\"\n\n[workspace.metadata.layr-retest]\n{sweep}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("lib-a/Cargo.toml"), "[package]\nname = \"lib-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n").unwrap();
    std::fs::write(
        dir.join("lib-a/src/lib.rs"),
        "pub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(super::a(), 1);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\napp-only = []\n\n[dependencies]\nlib-a = { path = \"../lib-a\" }\n",
    )
    .unwrap();
    std::fs::write(dir.join("app/src/main.rs"), "fn main() {\n    println!(\"{}\", lib_a::a());\n}\n").unwrap();
}

/// Finalized incremental sessions of each incremental folder of a profile folder (`s-*`, not a lock or a working folder).
fn sessions_per_folder(debug: &Path) -> BTreeMap<String, usize> {
    entries(debug, "incremental")
        .into_iter()
        .map(|f| {
            let n = entries(&debug.join("incremental"), &f).into_iter().filter(|e| e.starts_with("s-") && !e.ends_with(".lock") && !e.ends_with("-working")).count();
            (f, n)
        })
        .collect()
}

/// Hard-link counts and inode numbers of the files under `dir`, for a failure message.
fn link_table(dir: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if let Ok(m) = p.metadata() {
                out.push(format!("{} ino={} nlink={} mtime={}", p.strip_prefix(dir).unwrap_or(&p).display(), m.ino(), m.nlink(), m.mtime()));
            }
        }
    }
    out.sort();
    out.join("\n")
}

/// Names of the entries of `dir/sub`.
fn entries(dir: &Path, sub: &str) -> BTreeSet<String> {
    std::fs::read_dir(dir.join(sub)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

/// Cargo hashes of the entries of `dir/sub`: the text after the last `-` of the name before its first `.`.
fn hashes(dir: &Path, sub: &str) -> BTreeSet<String> {
    entries(dir, sub).iter().map(|n| n.rsplit_once('-').unwrap().1.split('.').next().unwrap().to_string()).collect()
}

/// The build ids of the object files of each unit in `dir/deps`: `<name>-<hash>.<cgu>.<build>.rcgu.o`.
fn object_builds(dir: &Path) -> std::collections::BTreeMap<String, BTreeSet<String>> {
    let mut out: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for n in entries(dir, "deps").into_iter().filter(|n| n.ends_with(".rcgu.o")) {
        let parts: Vec<&str> = n.split('.').collect();
        let hash = hash_of(parts[0]);
        // `<crate>-<hash>.<cgu>.<build>.rcgu.o` or `<crate>-<hash>.<crate>.<id>-cgu.<n>.rcgu.o`.
        let build = if parts.len() == 5 { parts[2].to_string() } else { parts[2].trim_end_matches("-cgu").to_string() };
        out.entry(hash).or_default().insert(build);
    }
    out
}

fn hash_of(name: &str) -> String {
    name.rsplit_once('-').unwrap().1.to_string()
}

/// Runs cargo in `dir` and returns its stderr, where cargo writes `Compiling` lines.
fn cargo(dir: &Path, args: &[&str]) -> String {
    sh(dir, &dir.join("unused-store"), "cargo", args).1
}

fn sweep_cmd(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["retest", "sweep"];
    all.extend_from_slice(args);
    let (_, err) = sh(dir, &dir.join("unused-store"), "layr", &all);
    err
}

/// Standard build, a build with a feature the standard command does not use, a changed function, then
/// the sweep: the feature build is gone, the standard units stay, and the next build compiles nothing.
#[test]
fn sweep_removes_the_units_that_no_standard_command_uses() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    let build = ["build", "--workspace", "--tests", "--bins"];
    let debug = ws.join("target/debug");

    cargo(&ws, &build);
    let (units, outputs) = (entries(&debug, ".fingerprint"), hashes(&debug, "deps"));

    cargo(&ws, &["build", "--workspace", "--tests", "--bins", "--features", "lib-a/extra"]);
    assert!(entries(&debug, ".fingerprint").len() > units.len());

    // A changed function keeps the unit hash: the build writes new object files of the same unit.
    std::fs::write(ws.join("lib-a/src/lib.rs"), std::fs::read_to_string(ws.join("lib-a/src/lib.rs")).unwrap().replace("    1\n", "    1 + 0\n")).unwrap();
    cargo(&ws, &build);

    // A dry run names the bytes and removes nothing.
    let before = entries(&debug, "deps");
    let err = sweep_cmd(&ws, &["--dry-run"]);
    assert!(err.contains("would free"), "{err}");
    assert_eq!(entries(&debug, "deps"), before);

    // Before the sweep, the changed unit has object files of two builds: in `deps` (macOS, split debug info
    // `unpacked`), or as two finalized incremental sessions of its folder (Linux, `off`: no object file in `deps`).
    assert!(
        object_builds(&debug).values().any(|b| b.len() > 1) || sessions_per_folder(&debug).values().any(|n| *n > 1),
        "{:?}\ndeps: {:?}\nincremental: {:?}",
        object_builds(&debug),
        entries(&debug, "deps"),
        entries(&debug, "incremental")
    );

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept"), "{err}");
    assert_eq!(entries(&debug, ".fingerprint"), units, "a non-live unit stays");
    assert_eq!(hashes(&debug, "deps"), outputs, "a unit of the feature build stays, or a live unit lost its newest files");
    // The one exception: a live unit keeps only the object files of its newest build.
    let builds = object_builds(&debug);
    assert!(builds.values().all(|b| b.len() == 1), "{builds:?}");
    assert!(sessions_per_folder(&debug).values().all(|n| *n <= 1), "{:?}", sessions_per_folder(&debug));

    let err = cargo(&ws, &build);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The focused command ran last: the units of the other standard commands survive the sweep.
#[test]
fn sweep_keeps_the_units_of_the_other_standard_commands() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-focus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(
        &ws,
        "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"], [\"test\", \"-p\", \"lib-a\", \"--features\", \"extra\", \"--no-run\"]]",
    );
    let build = ["build", "--workspace", "--tests", "--bins"];
    let lib_test = ["test", "-p", "lib-a", "--features", "extra", "--no-run"];
    let debug = ws.join("target/debug");

    cargo(&ws, &build);
    cargo(&ws, &lib_test);
    let (units, outputs) = (entries(&debug, ".fingerprint"), hashes(&debug, "deps"));

    // The focused command ran last and has units of its own.
    cargo(&ws, &["build", "-p", "app", "--features", "app-only", "--tests"]);
    assert!(entries(&debug, ".fingerprint").len() > units.len());
    sweep_cmd(&ws, &[]);

    assert_eq!(entries(&debug, ".fingerprint"), units);
    assert_eq!(hashes(&debug, "deps"), outputs);
    let err = cargo(&ws, &build);
    assert!(!err.contains("Compiling"), "the workspace build compiled:\n{err}");
    let err = cargo(&ws, &lib_test);
    assert!(!err.contains("Compiling"), "the lib test build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A project whose standard commands build the release profile only (no `target/debug`): the sweep sweeps
/// `target/release`, keeps the incremental data of the live units, and the next build compiles nothing.
#[test]
fn sweep_sweeps_a_release_only_target() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--release\", \"--workspace\"]]\nenv = { CARGO_INCREMENTAL = \"1\", RUSTC_WRAPPER = \"\" }");
    // The layr project's release profile (`debug = 1`): the objects of a bin stay in `deps` and link it to its incremental data.
    let manifest = std::fs::read_to_string(ws.join("Cargo.toml")).unwrap();
    std::fs::write(ws.join("Cargo.toml"), manifest + "\n[profile.release]\ndebug = 1\n").unwrap();
    // The machine may set a rustc wrapper (sccache), which refuses incremental compilation.
    let (env, target) = ([("CARGO_INCREMENTAL", "1"), ("RUSTC_WRAPPER", "")], ws.join("target"));
    let release = target.join("release");
    let build = ["build", "--release", "--workspace"];

    cargo_in(&ws, &target, &env, &build);
    let (units, outputs, inc) = (entries(&release, ".fingerprint"), hashes(&release, "deps"), entries(&release, "incremental"));

    cargo_in(&ws, &target, &env, &["build", "--release", "--workspace", "--features", "lib-a/extra"]);
    assert!(entries(&release, ".fingerprint").len() > units.len());
    // A changed function keeps the unit hash: the build writes new object files of the same unit.
    set_lib_a(&ws, "1 + 0");
    cargo_in(&ws, &target, &env, &build);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept"), "the release folder was not swept:\n{err}");
    assert_eq!(entries(&release, ".fingerprint"), units, "a non-live unit stays");
    assert_eq!(hashes(&release, "deps"), outputs, "a unit of the feature build stays, or a live unit lost its newest files");
    assert_eq!(entries(&release, "incremental"), inc, "the incremental data of a live unit was removed");
    let builds = object_builds(&release);
    assert!(builds.values().all(|b| b.len() == 1), "{builds:?}");

    let err = cargo_in(&ws, &target, &env, &build);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Modification times of the output files of a profile folder: a build that compiles a unit writes its
/// `.rlib`, `.rmeta`, objects and binaries in `deps`, `build` and `examples`. Left out: the files directly in
/// the folder (cargo re-links the uplifted binary `debug/<bin>` to the command it runs, with no compile),
/// cargo's fingerprint folder, the incremental sessions (the sweep prunes them), the lock files,
/// `invoked.timestamp` and the dep-info files (`.d`, rewritten by cargo on a build with nothing to compile).
fn output_times(debug: &Path) -> BTreeMap<PathBuf, std::time::SystemTime> {
    let mut out = BTreeMap::new();
    let mut todo = vec![debug.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != ".fingerprint" && n != "incremental") {
                    todo.push(p);
                }
            } else if p.parent() != Some(debug) && p.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                n != "invoked.timestamp" && !n.starts_with(".cargo-") && !n.ends_with(".d")
            }) {
                out.insert(p.clone(), p.metadata().unwrap().modified().unwrap());
            }
        }
    }
    out
}

/// Asserts that the sweep wrote no output file: every file that is left has its modification time from
/// `before`. A removed file is not a write: cargo removes the output of a stale unit before it stops.
fn assert_no_output_written(debug: &Path, before: &BTreeMap<PathBuf, std::time::SystemTime>, err: &str) {
    let written: Vec<_> = output_times(debug).into_iter().filter(|(p, t)| before.get(p) != Some(t)).map(|(p, _)| p).collect();
    assert!(written.is_empty(), "written or created:\n{written:?}\n{err}");
}

/// Runs cargo in `dir` with the target folder `target` and the variables `env`, and returns its stderr.
fn cargo_in(dir: &Path, target: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
    let o = run_limited(
        Command::new("cargo")
            .args(args)
            .current_dir(dir)
            .env_remove("RUSTFLAGS")
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_NET_OFFLINE", "true")
            .envs(env.iter().copied()),
    );
    assert!(o.status.success(), "cargo {args:?} failed:\n{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Rewrites the body of `a` in `lib-a`. A change of the source makes the units of `lib-a` stale.
fn set_lib_a(ws: &Path, body: &str) {
    let src = format!("pub fn a() -> i32 {{\n    {body}\n}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{\n        assert_eq!(super::a(), 1);\n    }}\n}}\n");
    std::fs::write(ws.join("lib-a/src/lib.rs"), src).unwrap();
}

/// The listing of a standard command finds a unit that is not fresh: the sweep stops at that unit,
/// compiles nothing, and removes nothing from the folder. The next build of the command makes it sweepable.
#[test]
fn sweep_skips_a_folder_with_a_unit_that_is_not_fresh() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(
        &ws,
        "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"], [\"build\", \"-p\", \"app\", \"--features\", \"app-only\", \"--tests\"]]",
    );
    let debug = ws.join("target/debug");
    cargo(&ws, &["build", "--workspace", "--tests", "--bins"]);
    // A stale unit that a sweep would remove, if it swept this folder.
    cargo(&ws, &["build", "--workspace", "--tests", "--bins", "--features", "lib-a/extra"]);
    let before = entries(&debug, ".fingerprint");
    let times = output_times(&debug);

    // The second standard command was never built: it is not fresh, so nothing is swept.
    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("sweep skipped") && err.contains("not fresh") && err.contains("run the standard build first"), "{err}");
    assert!(before.is_subset(&entries(&debug, ".fingerprint")), "a unit was removed:\n{err}");
    assert_no_output_written(&debug, &times, &err);

    // The second command built: the next sweep removes the stale unit.
    cargo(&ws, &["build", "-p", "app", "--features", "app-only", "--tests"]);
    let before_sweep = entries(&debug, ".fingerprint");
    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept target, freed"), "{err}");
    assert!(entries(&debug, ".fingerprint").len() < before_sweep.len());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A source change makes `target/debug` stale. The sweep skips it with the reason, and compiles nothing.
#[test]
fn sweep_skips_a_stale_target_and_compiles_nothing() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    let build = ["build", "--workspace", "--tests", "--bins"];
    let debug = ws.join("target/debug");
    cargo(&ws, &build);
    set_lib_a(&ws, "1 + 0");
    let times = output_times(&debug);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("sweep skipped") && err.contains("run the standard build first"), "{err}");
    assert_no_output_written(&debug, &times, &err);

    // The standard build makes the folder fresh, and then the sweep runs.
    cargo(&ws, &build);
    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept target, freed"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Another cargo holds the build lock of the folder: the sweep skips it at once and does not wait for that cargo.
#[test]
fn sweep_skips_a_busy_build_lock_without_waiting() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    let debug = ws.join("target/debug");
    cargo(&ws, &["build", "--workspace", "--tests", "--bins"]);
    // This test process holds the lock, so its pid is the holder that the sweep must name.
    let held = std::fs::File::options().append(true).open(debug.join(".cargo-lock")).unwrap();
    held.lock().unwrap();
    let times = output_times(&debug);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("build lock busy") && err.contains(&format!("pid {} (", std::process::id())), "{err}");
    // CI sets CARGO_TERM_COLOR=always. Escape codes in front of cargo's wait line must not hide the busy lock.
    let child = Command::new("layr")
        .args(["retest", "sweep"])
        .current_dir(&ws)
        .env("LAYR_ROOT", ws.join("unused-store")).env("LAYR_DIRECT", "1")
        .env("CARGO_TERM_COLOR", "always")
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let out = finish(child, LIMIT);
    let status = out.status;
    let colored = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(status.success(), "{colored}");
    assert!(colored.contains("build lock busy") && colored.contains("holder: pid"), "{colored}");
    drop(held);
    assert_no_output_written(&debug, &times, &err);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Without a `retest` command, `target/retest` is left as it is: no listing, no compile, no new folder.
#[test]
fn sweep_leaves_target_retest_alone_without_a_retest_command() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-noretest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    let build = ["build", "--workspace", "--tests", "--bins"];
    // The retest build comes first: a source change after it makes `target/retest` stale, and leaves `target` fresh.
    cargo_in(&ws, &ws.join("target/retest"), &[], &build);
    set_lib_a(&ws, "1 + 0");
    cargo(&ws, &build);
    let retest = ws.join("target/retest/debug");
    let times = output_times(&retest);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("target/retest: no sweep command; left as is"), "{err}");
    assert_eq!(output_times(&retest), times, "target/retest was compiled or changed");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Without a `sweep` list and without `target/retest`, a sweep creates nothing and compiles nothing.
#[test]
fn sweep_never_creates_target_retest() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-create-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    let build = ["build", "--workspace", "--tests", "--bins"];
    cargo(&ws, &build);
    let times = output_times(&ws.join("target/debug"));

    sweep_cmd(&ws, &[]);
    assert!(!ws.join("target/retest").exists());
    assert_eq!(output_times(&ws.join("target/debug")), times, "the sweep wrote or removed a file");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A `retest` command sweeps `target/retest` with the sweep list and the retest list, under the
/// environment of the retest build: a unit that neither list names goes, and the next retest build compiles nothing.
#[test]
fn sweep_sweeps_target_retest_with_a_retest_command() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-retest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(
        &ws,
        "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]\nretest = [[\"test\", \"--workspace\", \"--no-run\"]]\nenv = { RUSTC_WRAPPER = \"\" }",
    );
    // The variables that `layr retest` gives its cargo calls, so that the unit hashes match the sweep.
    let env = [
        ("RUSTFLAGS", "-C instrument-coverage"),
        ("CARGO_INCREMENTAL", "1"),
        ("RUSTC_WRAPPER", ""),
        ("CARGO_PROFILE_DEV_DEBUG", "line-tables-only"),
        ("CARGO_PROFILE_TEST_DEBUG", "line-tables-only"),
    ];
    let retest = ws.join("target/retest");
    cargo_in(&ws, &retest, &env, &["build", "--workspace", "--tests", "--bins"]);
    cargo_in(&ws, &retest, &env, &["test", "--workspace", "--no-run"]);
    cargo_in(&ws, &retest, &env, &["build", "--workspace", "--tests", "--bins", "--features", "lib-a/extra"]);
    let units = entries(&retest.join("debug"), ".fingerprint");

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept target/retest, freed"), "{err}");
    assert!(entries(&retest.join("debug"), ".fingerprint").len() < units.len(), "the feature unit stays");
    let err = cargo_in(&ws, &retest, &env, &["test", "--workspace", "--no-run"]);
    assert!(!err.contains("Compiling"), "the retest build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The sweep lists a standard command with a feature that the test run never built. Its listing
/// compiles the build script of the project and runs it. Cargo gives that run no profile file of its
/// own: the script wrote `default_*.profraw` into the project, and the stray cleanup removed it at the
/// end of the run. No file may reach the project, and the scratch folder of the profiles is gone.
#[test]
fn build_scripts_write_no_profile_into_the_project() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-buildrs-{}", std::process::id()));
    let src = tmp.join("src-project");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(
        src.join("Cargo.toml"),
        "[package]\nname = \"bsc\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[features]\nextra = []\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nsweep = [[\"build\", \"--features\", \"extra\"]]\n",
    )
    .unwrap();
    std::fs::write(src.join("build.rs"), "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n").unwrap();
    std::fs::write(
        src.join("src/lib.rs"),
        "pub fn a() -> i32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn ta() {\n        assert_eq!(super::a(), 1);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\ntarget regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    sh(&src, &tmp.join("unused"), "cargo", &["generate-lockfile", "--offline"]);
    let root = tmp.join("store");
    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    sh(&root.join("projects/app/lines/main"), &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");

    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("passed 1, failed 0"), "{err}");
    assert!(!err.contains("stray"), "{err}");
    let stray: Vec<_> = walk(&line).into_iter().filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    assert!(stray.is_empty(), "{stray:?}");
    assert!(!line.join("target/retest/profraw").exists());

    sh(&line, &root, "layr", &["retest", "sweep"]);
    let stray: Vec<_> = walk(&line).into_iter().filter(|p| p.extension().is_some_and(|e| e == "profraw")).collect();
    assert!(stray.is_empty(), "{stray:?}");
    assert!(!line.join("target/retest/profraw").exists());

    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Runs `cargo <args> --message-format json` in `dir` and returns the stdout (the unit messages) and stderr.
fn cargo_json(dir: &Path, target: &Path, args: &[&str]) -> (String, String) {
    let o = run_limited(
        Command::new("cargo")
            .args(args)
            .arg("--message-format")
            .arg("json")
            .current_dir(dir)
            .env_remove("RUSTFLAGS")
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_NET_OFFLINE", "true"),
    );
    assert!(o.status.success(), "cargo {args:?} failed:\n{}", String::from_utf8_lossy(&o.stderr));
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

/// Writes `n` bytes to `p`, creating the folders of `p`.
fn put(p: &Path, n: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; n]).unwrap();
}

/// Every kind of stale output at once: the outputs of a feature build, an orphan incremental folder, an unhashed
/// `.a` and unhashed objects of crates that no command lists, a stale top-level bin, a stale example and its copy.
/// After the sweep, only the outputs that the standard command names are left, and its next build compiles nothing.
#[test]
fn sweep_leaves_only_the_outputs_that_the_standard_command_names() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-allow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\", \"--examples\"]]");
    std::fs::create_dir_all(ws.join("app/examples")).unwrap();
    std::fs::write(ws.join("app/examples/demo.rs"), "fn main() {}\n").unwrap();
    let build = ["build", "--workspace", "--tests", "--bins", "--examples"];
    let debug = ws.join("target/debug");

    cargo(&ws, &build);
    // A feature build: its units are not listed by the standard command.
    cargo(&ws, &["build", "--workspace", "--tests", "--bins", "--examples", "--features", "lib-a/extra"]);
    // A changed source: the standard units build again, so a live unit has the objects of two builds.
    std::fs::write(ws.join("lib-a/src/lib.rs"), std::fs::read_to_string(ws.join("lib-a/src/lib.rs")).unwrap().replace("    1\n", "    1 + 0\n")).unwrap();
    cargo(&ws, &build);

    // Garbage that no command lists.
    put(&debug.join("libgone.a"), 4096);
    put(&debug.join("deps/libgone-0123456789abcdef.a"), 4096);
    put(&debug.join("deps/lib_a.0abc.0deadbe.rcgu.o"), 100);
    put(&debug.join("stale-bin"), 100);
    put(&debug.join("examples/stale-ex"), 100);
    put(&debug.join("examples/stale-ex-0123456789abcdef"), 100);
    put(&debug.join("incremental/orphan-zz9/s-1-2-abc/x.o"), 100);
    put(&debug.join("incremental/orphan-zz9/s-1-2-abc.lock"), 0);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept target, freed"), "{err}");

    for gone in ["libgone.a", "deps/libgone-0123456789abcdef.a", "deps/lib_a.0abc.0deadbe.rcgu.o", "stale-bin", "examples/stale-ex", "examples/stale-ex-0123456789abcdef", "incremental/orphan-zz9"] {
        assert!(!debug.join(gone).exists(), "{gone} stays");
    }
    // The outputs that the standard command names, and the hashes of its units.
    let (msgs, _) = cargo_json(&ws, &ws.join("target"), &build);
    let mut listed = BTreeSet::new();
    for line in msgs.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
        if line["reason"] == "compiler-artifact" {
            assert_eq!(line["fresh"], true, "{line}");
            for f in line["filenames"].as_array().unwrap().iter().filter_map(|f| f.as_str()).chain(line["executable"].as_str()) {
                listed.insert(f.to_string());
            }
        }
    }
    assert!(!listed.is_empty());
    for f in &listed {
        assert!(Path::new(f).exists(), "a listed output is gone: {f}");
    }
    // Every file of the profile folder is a listed output, cargo's lock, or an entry of a listed unit.
    let hash = |n: &str| n.split('.').next().unwrap().rsplit_once('-').map(|(_, h)| h.to_string()).filter(|h| h.len() == 16);
    let live: BTreeSet<String> = listed.iter().filter_map(|f| hash(Path::new(f).file_name().unwrap().to_str().unwrap())).collect();
    // A hashed file of a unit (a bin has a copy in the folder and a hashed file in `deps`) stays with its fingerprint.
    let fingerprints: BTreeSet<String> = entries(&debug, ".fingerprint").iter().filter_map(|n| hash(n)).collect();
    for sub in ["", "deps", "examples"] {
        for e in std::fs::read_dir(debug.join(sub)).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let ok = listed.contains(p.to_str().unwrap()) || name.starts_with(".cargo-") || (name.ends_with(".d") && listed.contains(p.with_extension("").to_str().unwrap())) || hash(&name).is_some_and(|h| fingerprints.contains(&h));
            assert!(ok, "{} stays but no listed unit needs it", p.display());
        }
    }
    // Every incremental folder that stays keeps one finalized session.
    for e in std::fs::read_dir(debug.join("incremental")).unwrap().flatten() {
        let sessions = entries(&e.path(), "").into_iter().filter(|n| n.starts_with("s-") && !n.ends_with(".lock")).count();
        assert_eq!(sessions, 1, "{:?}", e.path());
    }

    let err = cargo(&ws, &build);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let err = sweep_cmd(&ws, &["--dry-run"]);
    assert!(err.contains("would free 0.0 MB"), "a second sweep frees something:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A dylib used as a dev-dependency has an unhashed output (`libdy.dylib`) and unhashed objects. Its incremental
/// folder belongs to the live unit: a feature build of the dylib leaves a second folder, which the sweep removes.
/// The next build compiles nothing.
#[test]
fn sweep_keeps_the_incremental_folder_of_a_dylib_dev_dependency() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-dylib-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    std::fs::create_dir_all(ws.join("dy/src")).unwrap();
    std::fs::create_dir_all(ws.join("app/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"dy\", \"app\"]\nresolver = \"2\"\n\n[workspace.metadata.layr-retest]\nsweep = [[\"build\", \"--workspace\", \"--tests\"]]\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("dy/Cargo.toml"),
        "[package]\nname = \"dy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"dylib\"]\n\n[features]\nextra = []\n",
    )
    .unwrap();
    std::fs::write(ws.join("dy/src/lib.rs"), "pub fn d() -> i32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(super::d(), 2);\n    }\n}\n").unwrap();
    std::fs::write(ws.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dev-dependencies]\ndy = { path = \"../dy\" }\n").unwrap();
    std::fs::write(ws.join("app/src/lib.rs"), "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(1 + 1, 2);\n    }\n}\n").unwrap();
    let build = ["build", "--workspace", "--tests"];
    let debug = ws.join("target/debug");
    let dy_folders = || -> BTreeSet<String> { entries(&debug, "incremental").into_iter().filter(|n| n.starts_with("dy-")).collect() };

    cargo(&ws, &build);
    let live = dy_folders();
    // The feature build of the dylib replaces its output and objects; the standard build builds it again.
    cargo(&ws, &["build", "-p", "dy", "--features", "extra", "--tests"]);
    assert!(dy_folders().len() > live.len(), "{:?}", dy_folders());
    cargo(&ws, &build);

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept target, freed"), "{err}");
    assert_eq!(
        dy_folders(),
        live,
        "the sweep removed or kept the wrong incremental folder of a dylib unit\n{err}\ndeps: {:?}\nincremental: {:?}\n--- debug links:\n{}\n--- incremental links:\n{}",
        entries(&debug, "deps"),
        entries(&debug, "incremental"),
        link_table(&debug.join("deps")).lines().chain(link_table(&debug).lines().filter(|l| !l.contains('/'))).collect::<Vec<_>>().join("\n"),
        link_table(&debug.join("incremental"))
    );
    for f in &live {
        let session = entries(&debug.join("incremental").join(f), "");
        assert_eq!(session.iter().filter(|n| n.starts_with("s-") && !n.ends_with(".lock")).count(), 1, "{session:?}");
    }

    let err = cargo(&ws, &build);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Inode of each object file of the newest finalized session of `lib-a` (`incremental/lib_a-<id>/s-*`).
fn newest_session_objects(debug: &Path) -> BTreeMap<String, u64> {
    use std::os::unix::fs::MetadataExt;
    let folder = std::fs::read_dir(debug.join("incremental"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("lib_a-")))
        .unwrap();
    let session = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("s-") && !n.to_string_lossy().ends_with("-working"))
        })
        .max_by_key(|p| p.metadata().unwrap().modified().unwrap())
        .unwrap();
    std::fs::read_dir(session)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "o"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), p.metadata().unwrap().ino()))
        .collect()
}

/// A library with two modules and `debug = 0`: cargo keeps no object file in `deps`, so no file links the
/// incremental session of the library to its unit. The sweep keeps that session, and the build after a change
/// of one module reuses the object file of the other (the same inode in both sessions).
#[test]
fn sweep_keeps_the_incremental_session_of_a_library_without_debug_info() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-nodebug-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    std::fs::create_dir_all(ws.join("lib-a/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"lib-a\"]\nresolver = \"2\"\n\n[profile.dev]\ndebug = 0\n\n[workspace.metadata.layr-retest]\nsweep = [[\"build\", \"--workspace\", \"--lib\"]]\nenv = { CARGO_INCREMENTAL = \"1\", RUSTC_WRAPPER = \"\" }\n",
    )
    .unwrap();
    std::fs::write(ws.join("lib-a/Cargo.toml"), "[package]\nname = \"lib-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    std::fs::write(ws.join("lib-a/src/lib.rs"), "pub mod x;\npub mod y;\n").unwrap();
    std::fs::write(ws.join("lib-a/src/x.rs"), "pub fn fx() -> i32 {\n    1\n}\n").unwrap();
    std::fs::write(ws.join("lib-a/src/y.rs"), "pub fn fy() -> i32 {\n    2\n}\n").unwrap();
    let (env, target) = ([("CARGO_INCREMENTAL", "1"), ("RUSTC_WRAPPER", "")], ws.join("target"));
    let debug = target.join("debug");
    let build = ["build", "--workspace", "--lib"];

    cargo_in(&ws, &target, &env, &build);
    let folders = entries(&debug, "incremental");
    assert!(folders.iter().any(|f| f.starts_with("lib_a-")), "{folders:?}");
    assert!(entries(&debug, "deps").iter().all(|n| !n.ends_with(".o")), "this test needs a build without object files in deps");
    let before = newest_session_objects(&debug);

    sweep_cmd(&ws, &[]);
    assert_eq!(entries(&debug, "incremental"), folders, "the incremental session of a live library was removed");

    std::fs::write(ws.join("lib-a/src/x.rs"), "pub fn fx() -> i32 {\n    1 + 0\n}\n").unwrap();
    cargo_in(&ws, &target, &env, &build);
    let after = newest_session_objects(&debug);
    let reused = after.values().filter(|i| before.values().any(|j| j == *i)).count();
    assert!(
        reused >= 1,
        "the build after the change reused no object file of the previous session:\nbefore {before:?}\nafter {after:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The test binary of `lib-a` built with `debug = 0`: no object file and no `metadata.rmeta` link it to its incremental
/// folder. The sweep keeps the newest folder of the crate. After two changes and two sweeps one folder of the crate
/// remains, and the next build reuses its object files.
#[test]
fn sweep_keeps_one_incremental_folder_of_a_test_binary_without_debug_info() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-sweep-exe-nodebug-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    std::fs::create_dir_all(ws.join("lib-a/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"lib-a\"]\nresolver = \"2\"\n\n[profile.dev]\ndebug = 0\n\n[workspace.metadata.layr-retest]\nsweep = [[\"build\", \"--workspace\", \"--tests\"]]\nenv = { CARGO_INCREMENTAL = \"1\", RUSTC_WRAPPER = \"\" }\n",
    )
    .unwrap();
    std::fs::write(ws.join("lib-a/Cargo.toml"), "[package]\nname = \"lib-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    std::fs::write(ws.join("lib-a/src/lib.rs"), "pub mod x;\npub mod y;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(super::x::fx() + super::y::fy(), 3);\n    }\n}\n").unwrap();
    std::fs::write(ws.join("lib-a/src/x.rs"), "pub fn fx() -> i32 {\n    1\n}\n").unwrap();
    std::fs::write(ws.join("lib-a/src/y.rs"), "pub fn fy() -> i32 {\n    2\n}\n").unwrap();
    let (env, target) = ([("CARGO_INCREMENTAL", "1"), ("RUSTC_WRAPPER", "")], ws.join("target"));
    let debug = target.join("debug");
    let build = ["build", "--workspace", "--tests"];
    let crate_folders =
        || -> BTreeSet<String> { entries(&debug, "incremental").into_iter().filter(|n| n.starts_with("lib_a-")).collect() };
    let change_x =
        |body: &str| std::fs::write(ws.join("lib-a/src/x.rs"), format!("pub fn fx() -> i32 {{\n    {body}\n}}\n")).unwrap();

    cargo_in(&ws, &target, &env, &build);
    assert_eq!(crate_folders().len(), 1, "{:?}", crate_folders());
    sweep_cmd(&ws, &[]);
    assert_eq!(crate_folders().len(), 1, "the incremental folder of the live test binary was removed");

    change_x("1 + 0");
    cargo_in(&ws, &target, &env, &build);
    sweep_cmd(&ws, &[]);
    change_x("1 + 0 + 0");
    let before = newest_session_objects(&debug);
    cargo_in(&ws, &target, &env, &build);
    sweep_cmd(&ws, &[]);
    assert_eq!(crate_folders().len(), 1, "two sweeps left {:?}", crate_folders());

    change_x("1 + 0 + 0 + 0");
    cargo_in(&ws, &target, &env, &build);
    let after = newest_session_objects(&debug);
    let reused = after.values().filter(|i| before.values().any(|j| j == *i)).count();
    assert!(
        reused >= 1,
        "the build after the change reused no object file of the previous session:\nbefore {before:?}\nafter {after:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// `layr retest prune target` runs the cargo adapter by hand. The units of the feature build go, and the next
/// standard build compiles nothing.
#[test]
fn prune_command_runs_the_cargo_adapter_by_hand() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-prune-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    let build = ["build", "--workspace", "--tests", "--bins"];
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\", \"--tests\", \"--bins\"]]");
    cargo(&ws, &build);
    let debug = ws.join("target/debug");
    let units = entries(&debug, ".fingerprint");
    cargo(&ws, &["build", "--workspace", "--tests", "--bins", "--features", "lib-a/extra"]);
    assert!(entries(&debug, ".fingerprint").len() > units.len());

    let (out, _) = sh(&ws, &ws.join("unused-store"), "layr", &["retest", "prune", "target"]);
    assert!(out.contains("target: pruned with cargo"), "{out}");
    assert_eq!(entries(&debug, ".fingerprint"), units, "a non-live unit stays");

    let err = cargo(&ws, &build);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Each profile folder is pruned on its own. The source changes, so the release units are stale (nobody builds
/// release again) and the prune keeps `target/release`. The debug folder is fresh after the debug build: its garbage
/// (a feature build) goes, and the next debug build compiles nothing.
#[test]
fn prune_keeps_a_stale_release_folder_and_prunes_debug() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-profiles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--workspace\"], [\"build\", \"--release\", \"--workspace\"]]");
    let (debug, release) = (ws.join("target/debug"), ws.join("target/release"));
    cargo(&ws, &["build", "--workspace"]);
    cargo(&ws, &["build", "--release", "--workspace"]);
    let standard = entries(&debug, ".fingerprint");
    let release_units = entries(&release, ".fingerprint");
    cargo(&ws, &["build", "--workspace", "--features", "lib-a/extra"]);
    let with_extra = entries(&debug, ".fingerprint");
    let garbage: Vec<_> = with_extra.iter().filter(|u| !standard.contains(*u)).cloned().collect();
    assert!(!garbage.is_empty(), "the feature build made no debug unit");

    // The source changes: the release units are stale, the debug build makes the debug units fresh again.
    std::fs::write(
        ws.join("lib-a/src/lib.rs"),
        "pub fn a() -> i32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(super::a(), 2);\n    }\n}\n",
    )
    .unwrap();
    cargo(&ws, &["build", "--workspace"]);

    let (out, _) = sh(&ws, &ws.join("unused-store"), "layr", &["retest", "prune", "target"]);
    assert!(out.contains("target: pruned with cargo"), "debug was not pruned:\n{out}");
    let debug_after = entries(&debug, ".fingerprint");
    assert!(garbage.iter().all(|u| !debug_after.contains(u)), "a debug garbage unit stays");
    assert_eq!(debug_after, standard, "the debug folder is not the standard set");
    assert_eq!(entries(&release, ".fingerprint"), release_units, "the stale release folder was changed");

    let err = cargo(&ws, &["build", "--workspace"]);
    assert!(!err.contains("Compiling"), "the next debug build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A fresh release build is swept too: after the install build (`--release`) and its prune, the old release
/// variants are gone, and the next release build compiles nothing. A release folder that no command lists stays
/// as it is, so the release command must be in the list.
#[test]
fn prune_sweeps_a_fresh_release_folder() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"build\", \"--release\", \"--workspace\"]]");
    let release = ws.join("target/release");
    cargo(&ws, &["build", "--release", "--workspace"]);
    let standard = entries(&release, ".fingerprint");
    cargo(&ws, &["build", "--release", "--workspace", "--features", "lib-a/extra"]);
    assert!(entries(&release, ".fingerprint").len() > standard.len(), "the feature build made no release unit");

    let (out, _) = sh(&ws, &ws.join("unused-store"), "layr", &["retest", "prune", "target"]);
    assert!(out.contains("target: pruned with cargo"), "release was not pruned:\n{out}");
    assert_eq!(entries(&release, ".fingerprint"), standard, "the release folder keeps its garbage");

    let err = cargo(&ws, &["build", "--release", "--workspace"]);
    assert!(!err.contains("Compiling"), "the next release build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// `cargo test` builds the lib and the lib's test binary of one crate at the same time, each with its own
/// incremental folder. After a feature build, the sweep keeps the folders of the standard build (a swap of two
/// folders must not delete a live one), removes only what the standard command does not need, and the next
/// standard build compiles nothing.
#[test]
fn sweep_keeps_the_folders_of_a_lib_and_its_test_build() {
    let tmp = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-retest-lib-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    sweep_workspace(&ws, "sweep = [[\"test\", \"--workspace\", \"--no-run\"]]");
    let debug = ws.join("target/debug");
    cargo(&ws, &["test", "--workspace", "--no-run"]);
    let standard = entries(&debug, "incremental");
    assert!(!standard.is_empty(), "no incremental folder after the standard build");

    cargo(&ws, &["test", "--workspace", "--no-run", "--features", "lib-a/extra"]);
    let with_feature = entries(&debug, "incremental");
    assert!(with_feature.len() > standard.len(), "the feature build made no new incremental folder");

    let err = sweep_cmd(&ws, &[]);
    assert!(err.contains("swept"), "{err}");
    let after = entries(&debug, "incremental");
    assert!(standard.is_subset(&after), "the sweep removed a folder of the standard build: {standard:?} -> {after:?}");
    assert!(after.is_subset(&with_feature), "{after:?}");

    let err = cargo(&ws, &["test", "--workspace", "--no-run"]);
    assert!(!err.contains("Compiling"), "the next build compiled:\n{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}
