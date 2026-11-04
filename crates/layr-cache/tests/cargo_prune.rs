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

/// Every file and folder under `dir`, as paths relative to it.
fn names(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
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
