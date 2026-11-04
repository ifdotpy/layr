//! The entry point of the adapters: the detection of a folder and the setting `prune=<name>|none`.

use layr_cache::{prune, Outcome};

fn detect(folder: &std::path::Path) -> Option<&'static str> {
    layr_cache::adapters().iter().find(|a| a.detects(folder)).map(|a| a.name())
}
use std::path::PathBuf;

/// A project folder with the given files, and the path of its `folder`.
fn project(name: &str, files: &[&str], folder: &str) -> PathBuf {
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!("layr-cache-dispatch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join(folder)).unwrap();
    for f in files {
        std::fs::write(base.join(f), "").unwrap();
    }
    base.join(folder)
}

#[test]
fn detects_the_adapter_by_the_folder_name_and_its_project_file() {
    assert_eq!(detect(&project("cargo", &["Cargo.toml"], "target")), Some("cargo"));
    assert_eq!(detect(&project("pnpm", &["pnpm-lock.yaml"], "node_modules")), Some("pnpm"));
    // Without the project file, or with another name, no adapter matches.
    assert_eq!(detect(&project("nofile", &[], "target")), None);
    assert_eq!(detect(&project("othername", &["Cargo.toml", "pnpm-lock.yaml"], "build")), None);
}

#[test]
fn none_turns_pruning_off_and_an_unknown_name_is_an_error() {
    let folder = project("setting", &["Cargo.toml"], "target");
    match prune(Some("none"), &folder).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("prune=none"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
    let err = prune(Some("maven"), &folder).unwrap_err();
    assert!(format!("{err:#}").contains("unknown prune adapter 'maven'"), "{err:#}");
}

#[test]
fn a_name_runs_its_adapter_even_when_detection_would_not() {
    // The folder is named `build`: detection finds no adapter, but the setting names cargo, whose own check skips it.
    let folder = project("named", &["Cargo.toml"], "build");
    assert_eq!(detect(&folder), None);
    match prune(Some("cargo"), &folder).unwrap().1 {
        Outcome::Skipped { reason } => assert!(reason.contains("named target"), "{reason}"),
        other => panic!("expected a skip, got {other:?}"),
    }
}
