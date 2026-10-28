//! End to end: a tiny cargo project in a layr line with two tests on two functions.
//! Needs a `layr` with cache overlays (the one built from this repository: `cargo build -p layr`
//! and put `target/debug` first in PATH; the installed 0.4.0 of 2026-10-26 is too old),
//! `cargo nextest`, the registry package `hex` 0.4 in the cargo cache (the test runs offline), `cargo nextest` and the `llvm-tools` rustup component. The store is a temp
//! folder with its own LAYR_ROOT. Takes about 10 s.

use std::path::{Path, PathBuf};
use std::process::Command;

// `tc` reads a file, which coverage cannot track.
const LIB: &str = "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn ta() {\n        assert_eq!(a(), 1);\n    }\n\n    #[test]\n    fn tb() {\n        assert_eq!(b(), 2);\n    }\n\n    #[test]\n    fn th() {\n        assert_eq!(hex::encode([1u8]), \"01\");\n    }\n\n    #[test]\n    fn tc() {\n        assert_eq!(std::fs::read_to_string(\"data.txt\").unwrap(), \"ok\");\n    }\n}\n";

fn sh(dir: &Path, root: &Path, prog: &str, args: &[&str]) -> (String, String) {
    let o = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("LAYR_ROOT", root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{prog} {args:?} failed:\n{out}\n{err}");
    (out, err)
}

/// The newest record file of a line.
fn last_record(line: &Path) -> serde_json::Value {
    let newest = std::fs::read_dir(line.join(".layr-retest"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max_by_key(|p| p.metadata().unwrap().modified().unwrap())
        .unwrap();
    serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap()
}

fn retest(line: &Path, root: &Path, args: &[&str]) -> String {
    let t = std::time::Instant::now();
    let (_, err) = sh(line, root, env!("CARGO_BIN_EXE_layr-retest"), args);
    eprintln!("layr-retest {args:?}: {:.1}s\n{err}", t.elapsed().as_secs_f32());
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
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nhex = \"0.4\"\n",
    )
    .unwrap();
    std::fs::write(src.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(src.join("data.txt"), "ok").unwrap();
    std::fs::write(src.join(".layrattributes"), ".layr-retest regenerable\n").unwrap();
    std::fs::write(src.join(".gitignore"), "/target\n/.layr-retest\n").unwrap();
    let root = tmp.join("store");

    sh(&tmp, &root, "layr", &["init", "app", "--from", src.to_str().unwrap()]);
    let main: PathBuf = root.join("projects/app/lines/main");
    sh(&main, &root, "layr", &["line", "new", "work"]);
    let line = root.join("projects/app/lines/work");
    let lib = line.join("src/lib.rs");

    // 1. No record: every test runs.
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 4 selected"), "{err}");

    // The test that uses `hex` has the registry package in its record; the others do not.
    let rec: serde_json::Value = last_record(&line);
    let deps = |t: &str| rec["tests"][format!("tiny {t}")]["deps"].to_string();
    assert!(deps("tests::th").contains("hex-0.4."), "{rec}");
    assert_eq!(deps("tests::ta"), "[]", "{rec}");

    // 2. No change: nothing runs.
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 0 selected"), "{err}");

    // 3. Change the body of `a`: only `ta` runs.
    std::fs::write(&lib, LIB.replace("    1\n", "    1 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 1 selected"), "{err}");
    assert!(err.contains("ran 1, passed 1"), "{err}");

    // 4. A new function that no test calls: nothing runs.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("pub fn b()", "pub fn c() -> i32 {\n    3\n}\n\npub fn b()")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 0 selected"), "{err}");

    // 4b. An uncalled function above `a` shifts every line: nothing runs, the records must shift.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("pub fn d() -> i32 {{\n    4\n}}\n\n{s}")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 0 selected"), "{err}");

    // 4c. Edit `a` at its new place: `ta` must run, `tb` must not.
    let s = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, s.replace("    1 + 0\n", "    1 + 0 + 0\n")).unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 1 selected"), "{err}");
    assert!(err.contains("tiny tests::ta") && !err.contains("tiny tests::tb"), "{err}");

    // 5. A file that coverage cannot see changes: nothing runs.
    std::fs::write(line.join("data.txt"), "bad").unwrap();
    let err = retest(&line, &root, &["run"]);
    assert!(err.contains("4 tests, 0 selected"), "{err}");

    // 6. A full run runs all, fails `tc`, and reports the contradiction with the saved pass.
    let o = Command::new(env!("CARGO_BIN_EXE_layr-retest"))
        .args(["run", "--full"])
        .current_dir(&line)
        .env("LAYR_ROOT", &root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("4 selected") && err.contains("CONTRADICTION: tiny tests::tc"), "{err}");

    // 7. The records are in the cache overlay: a line made from this line takes them.
    std::fs::write(line.join("data.txt"), "ok").unwrap();
    sh(&line, &root, "layr", &["add", "Cargo.lock"]);
    sh(&line, &root, "layr", &["save", "-a", "-m", "work"]);
    retest(&line, &root, &["run"]);
    sh(&main, &root, "layr", &["line", "new", "work2", "work"]);
    let err = retest(&root.join("projects/app/lines/work2"), &root, &["run"]);
    assert!(err.contains("4 tests, 0 selected"), "{err}");

    // Saved states are immutable on macOS; unlock them before the delete.
    let _ = Command::new("chflags").args(["-R", "nouchg"]).arg(&tmp).status();
    let _ = std::fs::remove_dir_all(&tmp);
}
