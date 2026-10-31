//! Partial build: which workspace packages a run must build and list (docs/retest.md, "Partial build").
//! Pure rules; `main.rs` reads `cargo metadata` and calls them.

use crate::select::is_build_input;
use serde_json::Value;
use std::collections::BTreeSet;

/// A package of the workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct Pkg {
    pub name: String,
    /// Folder of the manifest, relative to the line root ("" for the root package).
    pub dir: String,
    /// Names of the workspace packages that this package depends on (any kind).
    pub deps: Vec<String>,
    /// True when cargo builds the bin targets of this package for its tests: it has a bin and an
    /// integration test. Another package's test can start such a bin from `target/`.
    pub spawnable_bin: bool,
}

/// Packages from `cargo metadata --no-deps`. `rel` makes a path relative to the line root.
pub fn load(meta: &Value, rel: impl Fn(&str) -> Option<String>) -> Vec<Pkg> {
    let list = meta["packages"].as_array().cloned().unwrap_or_default();
    let names: BTreeSet<&str> = list.iter().filter_map(|p| p["name"].as_str()).collect();
    let kind = |p: &Value, k: &str| p["targets"].as_array().is_some_and(|t| t.iter().any(|t| t["kind"].as_array().is_some_and(|ks| ks.iter().any(|x| x == k))));
    list.iter()
        .filter_map(|p| {
            let manifest = rel(p["manifest_path"].as_str()?)?;
            let dir = manifest.rsplit_once('/').map_or("", |(d, _)| d).to_string();
            let deps = p["dependencies"].as_array().into_iter().flatten().filter_map(|d| d["name"].as_str()).filter(|d| names.contains(d)).map(str::to_string).collect();
            Some(Pkg { name: p["name"].as_str()?.to_string(), dir, deps, spawnable_bin: kind(p, "bin") && kind(p, "test") })
        })
        .collect()
}

/// The package that owns `path`: the one with the longest folder that holds it.
fn owner<'a>(pkgs: &'a [Pkg], path: &str) -> Option<&'a Pkg> {
    pkgs.iter().filter(|p| p.dir.is_empty() || path.strip_prefix(&p.dir).is_some_and(|r| r.starts_with('/'))).max_by_key(|p| p.dir.len())
}

/// The package of a nextest binary id: `<package>` for a lib, `<package>::<target>` otherwise.
fn package_of_binary(id: &str) -> &str {
    id.split("::").next().unwrap_or(id)
}

/// The packages to build, or `None` for the full build. `stale` are the binary ids of the tests
/// that must run, `changed` the paths that differ from the baseline (both sides of a rename),
/// `excluded` the packages that the user's arguments exclude.
/// Rules: every package that holds a stale test; every package that owns a changed path (it can
/// hold a new test, and a compile error there must show); every package with a bin that tests
/// start and that depends, directly or not, on a changed package (the bin must be rebuilt).
/// A build input in `changed`, or a stale test of an unknown package, gives `None`.
pub fn build_set(pkgs: &[Pkg], stale: &[&str], changed: &[String], excluded: &[String]) -> Option<Vec<String>> {
    if changed.iter().any(|c| is_build_input(c)) {
        return None;
    }
    let mut set: BTreeSet<String> = BTreeSet::new();
    for id in stale {
        let name = package_of_binary(id);
        pkgs.iter().find(|p| p.name == name)?;
        set.insert(name.to_string());
    }
    let mut hit: BTreeSet<String> = changed.iter().filter_map(|c| owner(pkgs, c)).map(|p| p.name.clone()).collect();
    set.extend(hit.iter().cloned());
    loop {
        let more: Vec<String> = pkgs.iter().filter(|p| !hit.contains(&p.name) && p.deps.iter().any(|d| hit.contains(d))).map(|p| p.name.clone()).collect();
        if more.is_empty() {
            break;
        }
        hit.extend(more);
    }
    set.extend(pkgs.iter().filter(|p| p.spawnable_bin && hit.contains(&p.name)).map(|p| p.name.clone()));
    set.retain(|n| !excluded.contains(n));
    let all = pkgs.iter().filter(|p| !excluded.contains(&p.name)).count();
    (!set.is_empty() && set.len() < all).then(|| set.into_iter().collect())
}

/// Replace `--workspace` (or `--all`) and the `--exclude` arguments of the nextest arguments by
/// `-p <package>` for each package of `set`. `None` when the arguments do not select the workspace:
/// then the user's own selection stays.
pub fn narrow_args(extra: &[String], set: &[String]) -> Option<Vec<String>> {
    if !extra.iter().any(|a| a == "--workspace" || a == "--all") {
        return None;
    }
    let mut out = Vec::new();
    let mut it = extra.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" | "--all" => {}
            "--exclude" => drop(it.next()),
            e if e.starts_with("--exclude=") => {}
            _ => out.push(a.clone()),
        }
    }
    out.extend(set.iter().flat_map(|p| ["-p".to_string(), p.clone()]));
    Some(out)
}

/// The packages that the nextest arguments exclude.
pub fn excluded(extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = extra.iter();
    while let Some(a) = it.next() {
        if a == "--exclude" {
            out.extend(it.next().cloned());
        } else if let Some(e) = a.strip_prefix("--exclude=") {
            out.push(e.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, dir: &str, deps: &[&str], bin: bool) -> Pkg {
        Pkg { name: name.into(), dir: dir.into(), deps: deps.iter().map(|d| d.to_string()).collect(), spawnable_bin: bin }
    }

    /// base <- mid <- app (bin with tests); other is apart; sup is a bin with tests that depends on mid.
    fn ws() -> Vec<Pkg> {
        vec![
            pkg("base", "crates/base", &[], false),
            pkg("mid", "crates/mid", &["base"], false),
            pkg("app", "crates/app", &["mid"], true),
            pkg("other", "crates/other", &[], false),
            pkg("sup", "crates/sup", &["mid"], true),
            pkg("quiet", "crates/quiet", &[], true),
        ]
    }

    fn set(stale: &[&str], changed: &[&str]) -> Option<Vec<String>> {
        build_set(&ws(), stale, &changed.iter().map(|c| c.to_string()).collect::<Vec<_>>(), &[])
    }

    #[test]
    fn stale_test_package_and_changed_package_are_built() {
        assert_eq!(set(&["mid"], &["crates/mid/src/lib.rs"]).unwrap(), ["app", "mid", "sup"]);
        // A binary id of a non-lib target names its package.
        assert_eq!(set(&["other::it"], &[]).unwrap(), ["other"]);
    }

    #[test]
    fn a_changed_file_of_a_package_without_selected_tests_builds_it() {
        // A new test can sit in a file that no recorded test ran.
        assert_eq!(set(&[], &["crates/other/src/new.rs"]).unwrap(), ["other"]);
        // A deleted or renamed file counts by its old and new path alike.
        assert_eq!(set(&[], &["crates/other/a.rs", "crates/other/b.rs"]).unwrap(), ["other"]);
    }

    #[test]
    fn bins_that_tests_start_are_rebuilt_when_a_dependency_changed() {
        // base changed: app and sup depend on it, quiet does not.
        let s = set(&[], &["crates/base/src/lib.rs"]).unwrap();
        assert_eq!(s, ["app", "base", "sup"]);
        assert!(!s.contains(&"quiet".to_string()) && !s.contains(&"mid".to_string()));
    }

    #[test]
    fn full_build_when_every_package_is_needed_or_nothing_says_which() {
        assert_eq!(set(&["base", "quiet", "other", "mid"], &["crates/base/a.rs"]), None);
        assert_eq!(set(&[], &[]), None);
        assert_eq!(set(&["gone"], &[]), None);
    }

    #[test]
    fn full_build_for_build_inputs() {
        for p in ["Cargo.toml", "crates/other/Cargo.toml", "Cargo.lock", "crates/other/build.rs", ".cargo/config.toml"] {
            assert_eq!(set(&["other"], &[p]), None, "{p}");
        }
    }

    #[test]
    fn nested_package_folders_use_the_longest_match() {
        let p = vec![pkg("root", "", &[], false), pkg("inner", "crates/inner", &[], false), pkg("inner2", "crates/inner2", &[], false)];
        assert_eq!(owner(&p, "crates/inner2/src/a.rs").unwrap().name, "inner2");
        assert_eq!(owner(&p, "crates/inner/src/a.rs").unwrap().name, "inner");
        assert_eq!(owner(&p, "docs/a.md").unwrap().name, "root");
    }

    #[test]
    fn excluded_packages_are_never_built() {
        let ex = vec!["other".to_string()];
        let s = build_set(&ws(), &[], &["crates/other/a.rs".into(), "crates/base/a.rs".into()], &ex).unwrap();
        assert!(!s.contains(&"other".to_string()));
    }

    #[test]
    fn arguments_swap_workspace_and_exclude_for_packages() {
        let a: Vec<String> = ["--workspace", "--exclude", "x", "--profile", "fast", "--exclude=y"].map(String::from).to_vec();
        assert_eq!(excluded(&a), ["x", "y"]);
        let n = narrow_args(&a, &["p".into(), "q".into()]).unwrap();
        assert_eq!(n, ["--profile", "fast", "-p", "p", "-p", "q"]);
        // The user selects packages already: nothing to narrow.
        assert_eq!(narrow_args(&["-p".to_string(), "z".to_string()], &["p".into()]), None);
        assert_eq!(narrow_args(&[], &["p".into()]), None);
    }

    #[test]
    fn load_reads_cargo_metadata() {
        let m = serde_json::json!({"packages": [
            {"name": "a", "manifest_path": "/r/crates/a/Cargo.toml", "dependencies": [{"name": "serde"}, {"name": "b"}],
             "targets": [{"kind": ["lib"]}, {"kind": ["bin"]}, {"kind": ["test"]}]},
            {"name": "b", "manifest_path": "/r/Cargo.toml", "dependencies": [], "targets": [{"kind": ["bin"]}]}]});
        let p = load(&m, |p| p.strip_prefix("/r/").map(str::to_string));
        assert_eq!(p[0], pkg("a", "crates/a", &["b"], true));
        assert_eq!(p[1], pkg("b", "", &[], false));
    }
}
