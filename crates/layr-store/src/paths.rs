//! Relative object paths.

use anyhow::{bail, Result};
use std::path::{Component, Path};

/// Check a relative path: no absolute paths, no `..`, no empty components.
pub fn check_rel(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let p = Path::new(path);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => bail!("invalid path: {path}"),
        }
    }
    Ok(())
}

/// The ancestors of `rel`, nearest to the root first, without the root and without `rel` itself.
pub fn ancestors(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').map(move |(i, _)| &rel[..i])
}

#[cfg(test)]
mod tests {
    use super::ancestors;

    #[test]
    fn ancestors_run_from_the_root_down_and_leave_out_the_path() {
        assert_eq!(ancestors("a/bc/d.txt").collect::<Vec<_>>(), ["a", "a/bc"]);
        assert_eq!(ancestors("d.txt").count(), 0);
        assert_eq!(ancestors("").count(), 0);
    }
}
