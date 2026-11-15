//! Physical allocation scans; application attribution lives in fs::accounting.
use crate::fs::accounting::*;
use crate::fsutil::Space;
use anyhow::Result;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
};
pub(super) fn line_spaces(paths: &[PathBuf]) -> Result<Vec<Result<Space>>> {
    use std::collections::HashSet;
    let seen: Vec<_> = paths.iter().map(|_| Mutex::new(HashSet::new())).collect();
    let results: Vec<_> = paths.iter().map(|_| Mutex::new(Ok(Space::default()))).collect();
    let seed = paths.iter().enumerate().map(|(i, path)| (i, path.clone())).collect();
    crate::fsutil::work_queue(seed, |(i, path), add| {
        let walked = (|| -> Result<Space> {
            let mut space = Space::default();
            crate::fsutil::fiemap_visit_shallow(&path, &seen[i], &mut |child| add((i, child)), &mut |held, _, _| {
                space.add(held)
            })?;
            Ok(space)
        })();
        let mut result = results[i].lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match (result.as_mut(), walked) {
            (Ok(total), Ok(space)) => {
                total.total += space.total;
                total.unique += space.unique;
            }
            (_, Err(e)) => *result = Err(e),
            _ => {}
        }
    })?;
    Ok(results.into_iter().map(|r| r.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)).collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mounts {
    None,
    Below,
    Unknown,
}

impl Mounts {
    fn resolve_directories(self) -> bool {
        self != Self::None
    }
}

fn file_filesystem(
    dir_dev: u64,
    file_dev: u64,
    dir_fs: AllocationDomain,
    resolve: impl FnOnce() -> AllocationDomain,
) -> AllocationDomain {
    if dir_dev == file_dev {
        dir_fs
    } else {
        resolve()
    }
}

fn filesystem_fd(file: &std::fs::File, dev: u64) -> AllocationDomain {
    use std::os::fd::AsFd;
    crate::sys::filesystem_uuid(file.as_fd()).map(AllocationDomain::native).unwrap_or(AllocationDomain::device(dev))
}

fn filesystem(path: &Path) -> std::io::Result<AllocationDomain> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(file) = std::fs::File::open(path) {
        return Ok(filesystem_fd(&file, file.metadata()?.dev()));
    }
    Ok(AllocationDomain::device(path.metadata()?.dev()))
}

pub(super) fn allocations(root: &Path, parts: &[AllocationPart]) -> Result<AllocationReport> {
    let paths = parts.iter().map(|part| part.path.clone()).collect::<Vec<_>>();
    let domains = paths.iter().map(|path| domain_or_empty(path, filesystem)).collect::<std::io::Result<Vec<_>>>()?;
    let mut mounts = Mounts::Unknown;
    let plan =
        crate::pools::run(crate::pools::Priority::Background, crate::pools::Width::Items(std::num::NonZeroUsize::MIN), || {
            if let Some(mount) = crate::btrfs_scan::nested_mount(&paths)? {
                eprintln!("note: nested mount {}: walking files instead", mount.display());
                mounts = Mounts::Below;
                return Ok(None);
            }
            mounts = Mounts::None;
            crate::btrfs_scan::plan(root, &paths).map(Some)
        })?;
    let (tally, outside) = match plan {
        Ok(Some(plan)) => {
            let tally = AllocationTally::new(parts);
            let failures = Mutex::new(Vec::new());
            if let Some(width) = std::num::NonZeroUsize::new(plan.units.len()) {
                crate::pools::run(crate::pools::Priority::Background, crate::pools::Width::Items(width), || {
                    use rayon::prelude::*;
                    plan.units.into_par_iter().for_each(|unit| {
                        let mut batch = AllocationBatch::default();
                        let result = crate::btrfs_scan::visit(&unit, &mut |part, held| {
                            if let Some(domain) = domains[part] {
                                batch.add(part, domain, held);
                            }
                        });
                        match result {
                            Ok(()) => tally.commit(batch),
                            Err(error) => measurement_lock(&failures).push(error),
                        }
                    });
                })?;
            }
            let errors = failures.into_inner().map_err(|_| anyhow::anyhow!("allocation scan interrupted"))?;
            if let Some(error) = errors.first() {
                eprintln!("warning: bulk btrfs scan failed: {error}; walking files instead");
                (AllocationTally::new(parts), None)
            } else {
                (tally, Some(plan.outside))
            }
        }
        Ok(None) => (AllocationTally::new(parts), None),
        Err(error) => {
            if !matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::ENOTTY | libc::EINVAL)) {
                eprintln!("warning: bulk btrfs scan failed: {error}; walking files instead");
            }
            (AllocationTally::new(parts), None)
        }
    };
    let jobs = paths
        .iter()
        .enumerate()
        .filter(|(part, _)| domains[*part].is_some() && outside.as_ref().is_none_or(|outside| outside.contains(part)))
        .map(|(part, path)| (part, path.clone()))
        .collect();
    let seen: Vec<_> = parts.iter().map(|_| Mutex::new(HashSet::new())).collect();
    crate::fsutil::work_queue(jobs, |(part, path), enqueue| {
        let result = (|| -> Result<AllocationBatch> {
            let domain = if mounts.resolve_directories() { domain_or_empty(&path, filesystem)? } else { domains[part] };
            let Some(domain) = domain else { return Ok(AllocationBatch::default()) };
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&path)?;
            let mut batch = AllocationBatch::default();
            crate::fsutil::fiemap_visit_shallow(
                &path,
                &seen[part],
                &mut |child| enqueue((part, child)),
                &mut |held, file, dev| {
                    let file_domain = file_filesystem(metadata.dev(), dev, domain, || filesystem_fd(file, dev));
                    batch.add(part, file_domain, held);
                },
            )?;
            Ok(batch)
        })();
        tally.accept(part, result);
    })?;
    tally.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_mounts_require_directory_filesystem_checks() {
        use super::Mounts;
        assert!(!Mounts::None.resolve_directories());
        assert!(Mounts::Below.resolve_directories());
        assert!(Mounts::Unknown.resolve_directories());
    }

    #[test]
    fn file_mount_uses_its_own_filesystem() {
        use super::{file_filesystem, AllocationDomain};
        assert_eq!(
            file_filesystem(1, 1, AllocationDomain::device(1), || panic!("same device needs no lookup")),
            AllocationDomain::device(1)
        );
        assert_eq!(
            file_filesystem(1, 2, AllocationDomain::device(1), || AllocationDomain::device(2)),
            AllocationDomain::device(2)
        );
    }

    #[test]
    fn line_directory_units_count_hard_links_once() {
        let dir = std::env::temp_dir().join(format!("layr-du-parallel-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/file"), vec![1u8; 8192]).unwrap();
        std::fs::hard_link(dir.join("a/file"), dir.join("b/link")).unwrap();
        let expected = crate::fsutil::fiemap_space(&dir);
        let actual = super::line_spaces(&[dir.clone()]).unwrap().pop().unwrap();
        match expected {
            Ok(expected) => assert_eq!(actual.unwrap(), expected),
            Err(_) => assert!(actual.is_err()),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failing_line_does_not_skip_other_lines() {
        let dir = std::env::temp_dir().join(format!("layr-du-failing-part-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("first")).unwrap();
        std::fs::create_dir_all(dir.join("last")).unwrap();
        let paths = ["first", "missing", "last"].map(|name| dir.join(name));
        let results = super::line_spaces(&paths).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
