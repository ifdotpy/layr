//! The pnpm adapter on real pnpm projects. The packages are local tarballs (`file:` specs), so the
//! projects need no registry. The registry of every pnpm call is an address that refuses connections:
//! a call that reaches the network fails.

use layr_cache::{pnpm, prune, LocalHost, Outcome};

fn host() -> LocalHost {
    LocalHost::new(std::time::Duration::from_secs(600))
}
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Once;

/// The pnpm environment of the test process. The store is a folder of the temp dir, not the user's store.
fn env_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let store = std::env::temp_dir().join("layr-cache-pnpm-test-store");
        std::env::set_var("pnpm_config_store_dir", store);
        std::env::set_var("pnpm_config_registry", "http://127.0.0.1:9/");
        std::env::set_var("npm_config_registry", "http://127.0.0.1:9/");
    });
}

fn tmp(name: &str) -> PathBuf {
    env_once();
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-cache-pnpm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn pnpm(dir: &Path, args: &[&str]) -> Output {
    env_once();
    let o = Command::new("pnpm").args(args).current_dir(dir).stdin(Stdio::null()).output().unwrap();
    o
}

fn pnpm_ok(dir: &Path, args: &[&str]) {
    let o = pnpm(dir, args);
    assert!(
        o.status.success(),
        "pnpm {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

/// A package tarball `<name>-1.0.0.tgz` in `dir`. `preuninstall` is a shell command that pnpm runs when it removes the package.
fn tarball(dir: &Path, name: &str, preuninstall: Option<&str>) {
    let pkg = dir.join("src").join(name).join("package");
    std::fs::create_dir_all(&pkg).unwrap();
    let scripts = preuninstall.map(|s| format!(",\"scripts\":{{\"preuninstall\":\"{s}\"}}")).unwrap_or_default();
    std::fs::write(
        pkg.join("package.json"),
        format!("{{\"name\":\"{name}\",\"version\":\"1.0.0\",\"main\":\"index.js\"{scripts}}}"),
    )
    .unwrap();
    std::fs::write(pkg.join("index.js"), format!("module.exports = \"{name}\";\n")).unwrap();
    let o = Command::new("tar")
        .args(["-czf", &format!("{name}-1.0.0.tgz"), "-C", &format!("src/{name}"), "package"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success());
}

/// The project folder: the given dependencies, each a tarball of `dir`. Installed with `pnpm install`.
fn project(dir: &Path, deps: &[&str]) {
    let list: Vec<String> = deps.iter().map(|d| format!("\"{d}\":\"file:./{d}-1.0.0.tgz\"")).collect();
    write_manifest(dir, &list);
    pnpm_ok(dir, &["install", "--reporter=silent"]);
}

fn write_manifest(dir: &Path, list: &[String]) {
    std::fs::write(
        dir.join("package.json"),
        format!("{{\"name\":\"proj\",\"version\":\"1.0.0\",\"private\":true,\"dependencies\":{{{}}}}}", list.join(",")),
    )
    .unwrap();
}

/// The files of `dir` with their contents. A symbolic link is its target.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(dir).unwrap().to_path_buf();
            let m = p.symlink_metadata().unwrap();
            if m.file_type().is_symlink() {
                out.insert(rel, format!("link:{}", std::fs::read_link(&p).unwrap().display()).into_bytes());
            } else if m.is_dir() {
                todo.push(p);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

/// The entries of `node_modules/.pnpm` whose name starts with `prefix`.
fn virtual_store(nm: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(nm.join(".pnpm"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect()
}

#[test]
fn removes_a_package_that_the_lockfile_no_longer_has() {
    let dir = tmp("removed");
    tarball(&dir, "pkg-a", None);
    let marker = dir.join("ran-uninstall");
    tarball(&dir, "pkg-b", Some(&format!("touch {}", marker.display())));
    project(&dir, &["pkg-a", "pkg-b"]);
    write_manifest(&dir, &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string()]);
    pnpm_ok(&dir, &["install", "--lockfile-only", "--reporter=silent"]);
    let nm = dir.join("node_modules");
    assert!(nm.join("pkg-b").exists());

    match prune(None, &nm).unwrap().1 {
        Outcome::Pruned { freed } => {
            assert!(freed > 0);
        }
        other => panic!("expected a prune, got {other:?}"),
    }
    assert!(nm.join("pkg-b").symlink_metadata().is_err());
    assert!(virtual_store(&nm, "pkg-b@").is_empty(), "{:?}", virtual_store(&nm, "pkg-b@"));
    assert_eq!(std::fs::read_to_string(nm.join("pkg-a/index.js")).unwrap(), "module.exports = \"pkg-a\";\n");
    // The removal runs no lifecycle script of the removed package.
    assert!(!marker.exists(), "a lifecycle script ran");
}

#[test]
fn skips_a_lockfile_that_needs_a_package_the_store_lacks_and_leaves_node_modules_unchanged() {
    let dir = tmp("missing");
    tarball(&dir, "pkg-a", None);
    tarball(&dir, "pkg-b", None);
    tarball(&dir, "pkg-c", None);
    project(&dir, &["pkg-a", "pkg-b"]);
    // pkg-c is locked, then its tarball is gone: the store cannot supply it.
    write_manifest(
        &dir,
        &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string(), "\"pkg-c\":\"file:./pkg-c-1.0.0.tgz\"".to_string()],
    );
    pnpm_ok(&dir, &["install", "--lockfile-only", "--reporter=silent"]);
    std::fs::remove_file(dir.join("pkg-c-1.0.0.tgz")).unwrap();
    let nm = dir.join("node_modules");
    let before = tree(&nm);

    match prune(None, &nm).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("pnpm install"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(tree(&nm) == before, "node_modules changed");
}

#[test]
fn skips_a_stale_lockfile_and_leaves_node_modules_unchanged() {
    let dir = tmp("stale");
    tarball(&dir, "pkg-a", None);
    tarball(&dir, "pkg-b", None);
    project(&dir, &["pkg-a", "pkg-b"]);
    // package.json drops pkg-b, but the lockfile still has it.
    write_manifest(&dir, &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string()]);
    let nm = dir.join("node_modules");
    let before = tree(&nm);

    match prune(None, &nm).unwrap().1 {
        Outcome::Skipped { .. } => {}
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(tree(&nm) == before, "node_modules changed");
}

#[test]
fn skips_a_folder_without_a_lockfile_or_with_another_name() {
    let dir = tmp("nolock");
    tarball(&dir, "pkg-a", None);
    project(&dir, &["pkg-a"]);
    std::fs::remove_file(dir.join("pnpm-lock.yaml")).unwrap();
    let nm = dir.join("node_modules");
    let before = tree(&nm);
    match pnpm::prune(&host(), &nm) {
        Outcome::Skipped { reason } => assert!(reason.contains("pnpm-lock.yaml"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    std::fs::create_dir_all(dir.join("out")).unwrap();
    match pnpm::prune(&host(), &dir.join("out")) {
        Outcome::Skipped { reason } => assert!(reason.contains("node_modules"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(tree(&nm) == before);
}

/// The deadline stops pnpm mid-run: its process is killed, the copy is put back, and node_modules is unchanged.
/// (50 ms is shorter than a pnpm start, so the run stops before pnpm finishes whatever the machine.)
#[test]
fn a_deadline_stops_pnpm_and_leaves_node_modules_unchanged() {
    use layr_cache::LocalHost;
    let dir = tmp("deadline");
    tarball(&dir, "pkg-a", None);
    tarball(&dir, "pkg-b", None);
    project(&dir, &["pkg-a", "pkg-b"]);
    write_manifest(&dir, &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string()]);
    pnpm_ok(&dir, &["install", "--lockfile-only", "--reporter=silent"]);
    let nm = dir.join("node_modules");
    let before = tree(&nm);

    match pnpm::prune(&LocalHost::new(std::time::Duration::from_millis(50)), &nm) {
        Outcome::Skipped { .. } => {}
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(tree(&nm) == before, "node_modules changed");
    assert!(!nm.join("node_modules.layr-prune").exists() && !dir.join("node_modules.layr-prune").exists());
}

/// After a hard kill, recovery puts the copy back: node_modules is the copy, and the copy is gone.
#[test]
fn recover_puts_the_copy_back_after_a_hard_kill() {
    use layr_adapter::CacheAdapter;
    let dir = tmp("recover");
    tarball(&dir, "pkg-a", None);
    project(&dir, &["pkg-a"]);
    let nm = dir.join("node_modules");
    let before = tree(&nm);
    // What an interrupted prune leaves: the copy, and a damaged folder.
    std::process::Command::new("cp").args(["-R"]).arg(&nm).arg(dir.join("node_modules.layr-prune")).status().unwrap();
    std::fs::remove_dir_all(nm.join(".pnpm")).unwrap();
    assert!(!nm.join("pkg-a").exists());

    assert_eq!(layr_cache::pnpm::Pnpm.recover(&nm), Ok(true));
    assert!(tree(&nm) == before, "node_modules is not the copy");
    assert!(!dir.join("node_modules.layr-prune").exists());
    assert_eq!(layr_cache::pnpm::Pnpm.recover(&nm), Ok(false), "nothing left to restore");
}

#[test]
fn warm_installs_a_package_that_a_merge_added_and_a_failure_keeps_node_modules() {
    let dir = tmp("warm");
    tarball(&dir, "pkg-a", None);
    tarball(&dir, "pkg-b", None);
    project(&dir, &["pkg-a"]);
    let nm = dir.join("node_modules");
    // A merged change adds pkg-b to package.json and the lockfile; node_modules does not have it yet.
    write_manifest(&dir, &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string(), "\"pkg-b\":\"file:./pkg-b-1.0.0.tgz\"".to_string()]);
    pnpm_ok(&dir, &["install", "--lockfile-only", "--reporter=silent"]);
    assert!(nm.join("pkg-b").symlink_metadata().is_err());
    pnpm::warm(&host(), &nm).unwrap();
    assert_eq!(std::fs::read_to_string(nm.join("pkg-b/index.js")).unwrap(), "module.exports = \"pkg-b\";\n");
    assert!(!dir.join("node_modules.layr-prune").exists(), "the copy is removed");

    // A lockfile that needs a tarball that is gone: the install fails and node_modules stays as it was.
    tarball(&dir, "pkg-c", None);
    write_manifest(&dir, &["\"pkg-a\":\"file:./pkg-a-1.0.0.tgz\"".to_string(), "\"pkg-c\":\"file:./pkg-c-1.0.0.tgz\"".to_string()]);
    pnpm_ok(&dir, &["install", "--lockfile-only", "--reporter=silent"]);
    std::fs::remove_file(dir.join("pkg-c-1.0.0.tgz")).unwrap();
    let before = tree(&nm);
    assert!(pnpm::warm(&host(), &nm).is_err());
    assert!(tree(&nm) == before, "node_modules changed");
}
