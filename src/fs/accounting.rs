//! Shared attribution of physical allocation evidence to application parts.
use super::*;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
use crate::fsutil::Space;
use crate::fsutil::{ExtentKey, Held};
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AllocationDomain([u8; 17]);
impl AllocationDomain {
    #[cfg(target_os = "linux")]
    pub(crate) fn native(id: [u8; 16]) -> Self {
        let mut bytes = [0; 17];
        bytes[1..].copy_from_slice(&id);
        Self(bytes)
    }
    pub(crate) fn device(id: u64) -> Self {
        let mut bytes = [0; 17];
        bytes[0] = 1;
        bytes[1..9].copy_from_slice(&id.to_le_bytes());
        Self(bytes)
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Disappearance {
    Ignore,
    Report,
}
pub(crate) struct AllocationPart {
    pub path: PathBuf,
    pub disappearance: Disappearance,
}
#[derive(Default)]
pub(crate) struct PartSpace {
    pub own: u64,
    pub sole: u64,
}
pub(crate) struct AllocationReport {
    pub parts: Vec<Result<PartSpace>>,
    pub shared: u64,
}
#[derive(Clone, Copy)]
enum References {
    Part(usize),
    Several,
}
struct Extent {
    references: References,
    bytes: u64,
}
impl Extent {
    fn add(&mut self, other: Self) {
        self.references = match (self.references, other.references) {
            (References::Part(a), References::Part(b)) if a == b => References::Part(a),
            _ => References::Several,
        };
        self.bytes = self.bytes.max(other.bytes);
    }
}
#[derive(Default)]
pub(crate) struct AllocationBatch {
    private: HashMap<usize, u64>,
    shared: HashMap<(AllocationDomain, ExtentKey), Extent>,
}
impl AllocationBatch {
    pub(crate) fn add(&mut self, part: usize, domain: AllocationDomain, held: &Held) {
        *self.private.entry(part).or_default() += held.exclusive;
        for &(key, bytes) in held.shared {
            let extent = Extent { references: References::Part(part), bytes };
            match self.shared.entry((domain, key)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(extent);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().add(extent),
            }
        }
    }
    fn merge(&mut self, other: Self) {
        for (part, bytes) in other.private {
            *self.private.entry(part).or_default() += bytes;
        }
        for (key, extent) in other.shared {
            match self.shared.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(extent);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().add(extent),
            }
        }
    }
}
// A worker panic is propagated by the pool before a report can be returned. Recover guards
// during unwinding; finish also rejects poisoned counters instead of reporting partial totals.
pub(crate) fn measurement_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub(crate) struct AllocationTally<'a> {
    parts: &'a [AllocationPart],
    errors: Mutex<HashMap<usize, anyhow::Error>>,
    batch: Mutex<AllocationBatch>,
}
impl<'a> AllocationTally<'a> {
    pub(crate) fn new(parts: &'a [AllocationPart]) -> Self {
        Self { parts, errors: Mutex::new(HashMap::new()), batch: Mutex::new(AllocationBatch::default()) }
    }
    pub(crate) fn commit(&self, batch: AllocationBatch) {
        measurement_lock(&self.batch).merge(batch);
    }
    pub(crate) fn accept(&self, part: usize, result: Result<AllocationBatch>) {
        match result {
            Ok(batch) => self.commit(batch),
            Err(error) if matches!(self.parts[part].disappearance, Disappearance::Ignore) && vanished(&error) => {}
            Err(error) => {
                measurement_lock(&self.errors).insert(part, error);
            }
        }
    }
    pub(crate) fn finish(self) -> Result<AllocationReport> {
        let batch = self.batch.into_inner().map_err(|_| anyhow::anyhow!("allocation scan interrupted"))?;
        let mut errors = self.errors.into_inner().map_err(|_| anyhow::anyhow!("allocation scan interrupted"))?;
        let mut parts: Vec<_> = (0..self.parts.len())
            .map(|part| match errors.remove(&part) {
                Some(error) => Err(error),
                None => Ok(PartSpace { own: *batch.private.get(&part).unwrap_or(&0), sole: 0 }),
            })
            .collect();
        let mut shared = 0;
        for extent in batch.shared.into_values() {
            match extent.references {
                References::Part(part)
                    if parts.get_mut(part).is_some_and(|result| {
                        if let Ok(space) = result {
                            space.sole += extent.bytes;
                            true
                        } else {
                            false
                        }
                    }) => {}
                _ => shared += extent.bytes,
            }
        }
        Ok(AllocationReport { parts, shared })
    }
}
pub(crate) fn vanished(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        || error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT)
}
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
pub(crate) fn parallel_spaces(paths: &[PathBuf], measure: fn(&Path) -> Result<Space>) -> Result<Vec<Result<Space>>> {
    let Some(width) = std::num::NonZeroUsize::new(paths.len()) else { return Ok(Vec::new()) };
    Ok(crate::pools::run(crate::pools::Priority::Background, crate::pools::Width::Items(width), || {
        use rayon::prelude::*;
        paths.par_iter().map(|path| measure(path)).collect()
    })?)
}
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
pub(crate) fn directory_units(path: &Path) -> Result<Vec<PathBuf>> {
    let children = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    if !children.is_empty() && children.iter().all(|child| child.file_type().is_ok_and(|kind| kind.is_dir())) {
        Ok(children.into_iter().map(|child| child.path()).collect())
    } else {
        Ok(vec![path.to_path_buf()])
    }
}
pub(crate) fn domain_or_empty(
    path: &Path,
    probe: fn(&Path) -> std::io::Result<AllocationDomain>,
) -> std::io::Result<Option<AllocationDomain>> {
    match probe(path) {
        Ok(domain) => Ok(Some(domain)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
pub(crate) fn directory_allocations(
    parts: &[AllocationPart],
    domain: fn(&Path) -> std::io::Result<AllocationDomain>,
    units: fn(&Path) -> Result<Vec<PathBuf>>,
    visit: fn(&Path, &mut dyn FnMut(&Held)) -> Result<()>,
) -> Result<AllocationReport> {
    let tally = AllocationTally::new(parts);
    let mut jobs = Vec::new();
    for (part, request) in parts.iter().enumerate() {
        let planned = (|| -> Result<_> {
            let Some(domain) = domain_or_empty(&request.path, domain)? else { return Ok(Vec::new()) };
            Ok(units(&request.path)?.into_iter().map(|path| (part, domain, path)).collect::<Vec<_>>())
        })();
        match planned {
            Ok(units) => jobs.extend(units),
            Err(error) => tally.accept(part, Err(error)),
        }
    }
    if let Some(width) = std::num::NonZeroUsize::new(jobs.len()) {
        crate::pools::run(crate::pools::Priority::Background, crate::pools::Width::Items(width), || {
            use rayon::prelude::*;
            jobs.into_par_iter().for_each(|(part, domain, path)| {
                let mut batch = AllocationBatch::default();
                let result = visit(&path, &mut |held| batch.add(part, domain, held)).map(|()| batch);
                tally.accept(part, result);
            })
        })?;
    }
    tally.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(id: u64) -> ExtentKey {
        #[cfg(target_os = "linux")]
        {
            ExtentKey::Disk(id)
        }
        #[cfg(target_os = "macos")]
        {
            ExtentKey::ApfsClone(id)
        }
    }
    #[test]
    fn attribution_deduplicates_extents_and_separates_allocation_domains() -> Result<()> {
        let parts = (0..3)
            .map(|index| AllocationPart { path: PathBuf::from(index.to_string()), disappearance: Disappearance::Report })
            .collect::<Vec<_>>();
        let tally = AllocationTally::new(&parts);
        let domain = AllocationDomain::device(1);
        for (part, domain, private, extents) in [
            (0, domain, 10, vec![(key(3), 100)]),
            (0, domain, 0, vec![(key(3), 80)]),
            (1, domain, 20, vec![(key(3), 80), (key(4), 40)]),
            (2, AllocationDomain::device(2), 5, vec![(key(3), 70)]),
        ] {
            let mut batch = AllocationBatch::default();
            batch.add(part, domain, &Held { alloc: 0, own: 0, exclusive: private, shared: &extents });
            tally.accept(part, Ok(batch));
        }
        let report = tally.finish()?;
        assert_eq!(report.shared, 100);
        let spaces =
            report.parts.into_iter().map(|space| space.map(|space| (space.own, space.sole))).collect::<Result<Vec<_>>>()?;
        assert_eq!(spaces, [(10, 0), (20, 40), (5, 70)]);
        Ok(())
    }
    #[test]
    fn disappearing_parts_do_not_hide_permission_or_io_failures() -> Result<()> {
        let parts = [
            AllocationPart { path: PathBuf::from("trash"), disappearance: Disappearance::Ignore },
            AllocationPart { path: PathBuf::from("protected"), disappearance: Disappearance::Report },
        ];
        let tally = AllocationTally::new(&parts);
        tally.accept(0, Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()));
        tally.accept(1, Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()));
        let report = tally.finish()?;
        assert!(report.parts[0].is_ok());
        assert!(report.parts[1].is_err());
        Ok(())
    }
}
