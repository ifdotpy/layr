//! The npm adapter on a real npm project. The packages are local tarballs (`file:` specs), and the registry is an
//! address that refuses connections: a call that reaches the network fails.

use layr_adapter::CacheAdapter;
use layr_cache::{pnpm, LocalHost, Outcome};
use std::path::{Path, PathBuf};
use std::process::Command;

fn host() -> LocalHost {
    LocalHost::new(std::time::Duration::from_secs(600))
}

fn tmp(name: &str) -> PathBuf {
    std::env::set_var("npm_config_registry", "http://127.0.0.1:9/");
    std::env::set_var("npm_config_cache", std::env::temp_dir().join("layr-cache-npm-test-cache"));
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-cache-npm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn npm_ok(dir: &Path, args: &[&str]) {
    let o = Command::new("npm").args(args).args(["--no-audit", "--no-fund"]).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "npm {args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
}

fn tarball(dir: &Path, name: &str) {
    let pkg = dir.join("src").join(name).join("package");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("package.json"), format!("{{\"name\":\"{name}\",\"version\":\"1.0.0\",\"main\":\"index.js\"}}"))
        .unwrap();
    std::fs::write(pkg.join("index.js"), format!("module.exports = \"{name}\";\n")).unwrap();
    let o = Command::new("tar")
        .args(["-czf", &format!("{name}-1.0.0.tgz"), "-C", &format!("src/{name}"), "package"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success());
}

fn manifest(dir: &Path, deps: &[&str]) {
    let list: Vec<String> = deps.iter().map(|d| format!("\"{d}\":\"file:./{d}-1.0.0.tgz\"")).collect();
    std::fs::write(
        dir.join("package.json"),
        format!("{{\"name\":\"proj\",\"version\":\"1.0.0\",\"private\":true,\"dependencies\":{{{}}}}}", list.join(",")),
    )
    .unwrap();
}

#[test]
fn npm_prunes_a_removed_package_and_warm_installs_an_added_one() {
    let dir = tmp("flow");
    tarball(&dir, "pkg-a");
    tarball(&dir, "pkg-b");
    manifest(&dir, &["pkg-a", "pkg-b"]);
    npm_ok(&dir, &["install"]);
    let nm = dir.join("node_modules");
    assert!(pnpm::Npm.detects(&nm) && !pnpm::Pnpm.detects(&nm));

    // The lockfile drops pkg-b: the prune removes it, offline.
    manifest(&dir, &["pkg-a"]);
    npm_ok(&dir, &["install", "--package-lock-only"]);
    // The lockfile is a file of the line: the prune leaves its bytes and its time, also when npm would write it in
    // its own format.
    let lock = dir.join("package-lock.json");
    let compact =
        serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(&std::fs::read(&lock).unwrap()).unwrap()).unwrap();
    std::fs::write(&lock, &compact).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    std::fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
    match pnpm::Npm.prune(&host(), &nm) {
        Outcome::Pruned { .. } => {}
        other => panic!("expected a prune, got {other:?}"),
    }
    assert_eq!(std::fs::read(&lock).unwrap(), compact, "the prune keeps the bytes of the lockfile");
    assert_eq!(std::fs::metadata(&lock).unwrap().modified().unwrap(), old, "and its time");
    assert!(nm.join("pkg-b").symlink_metadata().is_err(), "pkg-b stays");
    assert!(nm.join("pkg-a/index.js").is_file());

    // A merged change adds pkg-b again: the warm step installs it, and leaves no copy.
    manifest(&dir, &["pkg-a", "pkg-b"]);
    npm_ok(&dir, &["install", "--package-lock-only"]);
    pnpm::Npm.warm(&host(), &nm).unwrap();
    assert!(nm.join("pkg-b/index.js").is_file());
    assert!(!dir.join("node_modules.layr-prune").exists());
}
