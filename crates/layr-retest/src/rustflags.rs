//! Rust flags of the instrumented build. Cargo reads the flags from the first set source of:
//! `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, `target.<triple>.rustflags`, `build.rustflags`.
//! `RUSTFLAGS` replaces every config value, so retest sets it only when the caller did. The
//! `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` and `CARGO_BUILD_RUSTFLAGS` variables merge with the config
//! values of their section, so retest appends to the variable of the section that cargo reads.
//! docs/retest.md, "Project rustflags", has the measurements.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// The cargo config files that apply to `root`: `$CARGO_HOME/config.toml`, then each folder from the
/// top down to `root`. Each folder holds `.cargo/config.toml` and the older `.cargo/config`.
pub fn config_files(root: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Table>> {
    let home = env("CARGO_HOME").map(PathBuf::from).or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let mut dirs: Vec<PathBuf> = root.ancestors().map(|d| d.join(".cargo")).collect();
    dirs.reverse();
    let mut files = Vec::new();
    for dir in home.into_iter().chain(dirs) {
        for name in ["config.toml", "config"] {
            let path = dir.join(name);
            if path.is_file() {
                let text = std::fs::read_to_string(&path)?;
                files.push(toml::from_str(&text).with_context(|| path.display().to_string())?);
            }
        }
    }
    Ok(files)
}

/// The variable and value that give the instrumented cargo calls the rustflags that cargo would use
/// for `host` in `files`, with `flags` appended. `env` reads an environment variable.
pub fn env_with_flags(
    env: &dyn Fn(&str) -> Option<String>,
    files: &[Table],
    host: &str,
    flags: &[&str],
) -> Result<(String, String)> {
    let cfg_flags = files
        .iter()
        .filter_map(|f| f.get("target")?.as_table())
        .flat_map(|t| t.iter())
        .any(|(k, v)| k.starts_with("cfg(") && v.as_table().is_some_and(|t| t.contains_key("rustflags")));
    if cfg_flags {
        bail!("layr retest cannot read rustflags under target.'cfg(...)'; move them to target.<triple> or build.rustflags");
    }
    let triple = match env("CARGO_BUILD_TARGET") {
        Some(t) => t,
        None => match files.iter().find_map(|f| f.get("build")?.get("target")) {
            Some(v) => v.as_str().context("build.target must be a string for layr retest")?.to_string(),
            None => host.to_string(),
        },
    };
    let target_var = format!("CARGO_TARGET_{}_RUSTFLAGS", triple.to_uppercase().replace(['-', '.'], "_"));
    let has_target_flags = files.iter().any(|f| {
        f.get("target").and_then(|t| t.get(&triple)).and_then(Value::as_table).is_some_and(|t| t.contains_key("rustflags"))
    });
    let plain = |name: &str, base: Option<String>| (name.to_string(), join(base, " ", flags));
    if let Some(v) = env("CARGO_ENCODED_RUSTFLAGS") {
        return Ok(("CARGO_ENCODED_RUSTFLAGS".to_string(), join(Some(v), "\u{1f}", flags)));
    }
    if let Some(v) = env("RUSTFLAGS") {
        return Ok(plain("RUSTFLAGS", Some(v)));
    }
    if has_target_flags || env(&target_var).is_some() {
        return Ok(plain(&target_var, env(&target_var)));
    }
    Ok(plain("CARGO_BUILD_RUSTFLAGS", env("CARGO_BUILD_RUSTFLAGS")))
}

/// `base` followed by `words`, joined with `sep`. An empty `base` adds nothing.
fn join(base: Option<String>, sep: &str, words: &[&str]) -> String {
    let base = base.unwrap_or_default();
    let mut parts: Vec<&str> = Vec::new();
    if !base.is_empty() {
        parts.push(&base);
    }
    parts.extend_from_slice(words);
    parts.join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "aarch64-apple-darwin";
    const TGT: &str = "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS";
    const FLAGS: &[&str] = &["-C", "instrument-coverage"];

    fn files(text: &str) -> Vec<Table> {
        vec![toml::from_str(text).unwrap()]
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn set(pairs: &[(&str, &str)], text: &str) -> Result<(String, String)> {
        env_with_flags(&env(pairs), &files(text), HOST, FLAGS)
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn build_section_gets_the_flags_through_its_variable() {
        // The config value stays: cargo merges the variable with it.
        let got = set(&[], "[build]\nrustflags = [\"--cfg\", \"projflag\"]\n").unwrap();
        assert_eq!(got, pair("CARGO_BUILD_RUSTFLAGS", "-C instrument-coverage"));
    }

    #[test]
    fn user_build_variable_is_kept() {
        let got = set(&[("CARGO_BUILD_RUSTFLAGS", "-Dwarnings")], "").unwrap();
        assert_eq!(got, pair("CARGO_BUILD_RUSTFLAGS", "-Dwarnings -C instrument-coverage"));
    }

    #[test]
    fn target_section_wins_over_build_section() {
        // Cargo reads target.<triple>.rustflags and ignores build.rustflags when both exist.
        let text = "[build]\nrustflags = [\"--cfg\", \"bld\"]\n[target.aarch64-apple-darwin]\nrustflags = [\"--cfg\", \"tgt\"]\n";
        assert_eq!(set(&[], text).unwrap(), pair(TGT, "-C instrument-coverage"));
    }

    #[test]
    fn target_variable_alone_selects_the_target_section() {
        let got = set(&[(TGT, "-Dwarnings")], "[build]\nrustflags = [\"--cfg\", \"bld\"]\n").unwrap();
        assert_eq!(got, pair(TGT, "-Dwarnings -C instrument-coverage"));
    }

    #[test]
    fn rustflags_variable_is_kept_whole() {
        let got = set(&[("RUSTFLAGS", "-Dwarnings")], "[build]\nrustflags = [\"--cfg\", \"projflag\"]\n").unwrap();
        assert_eq!(got, pair("RUSTFLAGS", "-Dwarnings -C instrument-coverage"));
    }

    #[test]
    fn empty_rustflags_variable_still_replaces_config() {
        let got = set(&[("RUSTFLAGS", "")], "").unwrap();
        assert_eq!(got, pair("RUSTFLAGS", "-C instrument-coverage"));
    }

    #[test]
    fn encoded_variable_is_kept_and_wins_over_rustflags() {
        let got = set(&[("CARGO_ENCODED_RUSTFLAGS", "-Dwarnings\u{1f}-C\u{1f}target-cpu=native")], "").unwrap();
        assert_eq!(
            got,
            pair("CARGO_ENCODED_RUSTFLAGS", "-Dwarnings\u{1f}-C\u{1f}target-cpu=native\u{1f}-C\u{1f}instrument-coverage")
        );
    }

    #[test]
    fn build_target_key_selects_its_triple() {
        let text = "[build]\ntarget = \"x86_64-apple-darwin\"\n[target.x86_64-apple-darwin]\nrustflags = [\"--cfg\", \"x\"]\n";
        let got = set(&[], text).unwrap();
        assert_eq!(got, pair("CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS", "-C instrument-coverage"));
    }

    #[test]
    fn cfg_target_section_with_flags_is_refused() {
        // It may match the host and shadow build.rustflags, which retest cannot evaluate.
        assert!(set(&[], "[target.'cfg(unix)']\nrustflags = [\"--cfg\", \"u\"]\n").is_err());
    }
}
