//! Shared validation of live reads. Native adapters return observations and may retain a
//! private scan cache; they never receive or execute an application read callback.
use super::*;
use crate::status_cache::FileFingerprint;
use std::{
    any::Any,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

pub(crate) enum ReadPolicy {
    Capture,
    Validate,
}
pub(crate) struct ReadObservation {
    pub entries: Vec<BTreeMap<String, FileFingerprint>>,
    _lease: Box<dyn Any>,
}
impl ReadObservation {
    pub(crate) fn new(entries: Vec<BTreeMap<String, FileFingerprint>>, lease: Box<dyn Any>) -> Self {
        Self { entries, _lease: lease }
    }
}
type Touched = Vec<(PathBuf, BTreeSet<String>)>;
thread_local! {static TOUCHED:RefCell<Touched>=const {RefCell::new(Vec::new())};}
struct Tracking {
    previous: Touched,
}
impl Tracking {
    fn start(roots: &[&Path]) -> Self {
        Self { previous: TOUCHED.replace(roots.iter().map(|root| (root.to_path_buf(), BTreeSet::new())).collect()) }
    }
}
impl Drop for Tracking {
    fn drop(&mut self) {
        let touched = TOUCHED.replace(std::mem::take(&mut self.previous));
        for (root, paths) in touched {
            note_read(&root, paths);
        }
    }
}
pub(crate) fn note_read(root: &Path, paths: impl IntoIterator<Item = impl AsRef<str>>) {
    TOUCHED.with_borrow_mut(|touched| {
        if let Some((_, set)) = touched.iter_mut().find(|(path, _)| path == root) {
            set.extend(paths.into_iter().map(|path| path.as_ref().to_owned()));
        }
    });
}

pub(crate) fn validated_read<T>(
    roots: &[&Path],
    selection: Option<&BTreeSet<String>>,
    read: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    let observations = host().observations;
    if matches!(observations.read_policy(), ReadPolicy::Capture) {
        return Ok(None);
    }
    validated_read_with(observations, roots, selection, read)
}
fn validated_read_with<T>(
    observations: &dyn Observations,
    roots: &[&Path],
    selection: Option<&BTreeSet<String>>,
    read: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    let _trace = crate::trace::span("validated filesystem read");
    let before = observations.begin_read(roots, selection)?;
    if before.entries.len() != roots.len()
        || before.entries.iter().any(|entries| entries.values().any(|entry| !entry.verifiable()))
    {
        return Ok(None);
    }
    let _tracking = Tracking::start(roots);
    let result = read();
    let touched = TOUCHED.with_borrow(Clone::clone);
    for ((root, expected), (_, paths)) in roots.iter().zip(&before.entries).zip(touched) {
        if selection.is_some_and(|selection| {
            paths
                .iter()
                .any(|path| !selection.iter().any(|selected| path == selected || path.starts_with(&format!("{selected}/"))))
        }) {
            return Ok(None);
        }
        let current = match observations.scan(root, Some(&paths)) {
            Ok(current) => current,
            Err(_) => return Ok(None),
        };
        if paths
            .iter()
            .any(|path| expected.get(path) != current.get(path) || current.get(path).is_some_and(|entry| !entry.verifiable()))
        {
            return Ok(None);
        }
    }
    result.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_cache::{FileStat, ObservationEvidence, Timestamp};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    struct Evidence {
        entries: Mutex<BTreeMap<String, FileFingerprint>>,
        scans: AtomicUsize,
    }
    impl Evidence {
        fn new() -> Self {
            let stamp = FileFingerprint::Verified {
                stat: FileStat {
                    dev: 1,
                    ino: 2,
                    size: 3,
                    mode: 0o100644,
                    uid: 1000,
                    gid: 1000,
                    nlink: 1,
                    mtime: Timestamp { seconds: 1, nanos: 0 },
                    ctime: Timestamp { seconds: 1, nanos: 0 },
                },
                evidence: Some(ObservationEvidence::Counter([1, 2, 0])),
            };
            Self { entries: Mutex::new(BTreeMap::from([("file".into(), stamp)])), scans: AtomicUsize::new(0) }
        }
        fn change(&self) {
            self.entries.lock().unwrap().insert("file".into(), FileFingerprint::Unverifiable);
        }
    }
    impl Observations for Evidence {
        fn read_policy(&self) -> ReadPolicy {
            ReadPolicy::Validate
        }
        fn stage_identity(&self, _: &Path) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn scan(&self, _: &Path, paths: Option<&BTreeSet<String>>) -> Result<BTreeMap<String, FileFingerprint>> {
            self.scans.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| paths.is_none_or(|paths| paths.contains(*path)))
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect())
        }
    }
    #[test]
    fn validates_touched_paths_and_rejects_mapped_or_changing_entries() -> Result<()> {
        let evidence = Evidence::new();
        let root = Path::new("/observed");
        assert_eq!(
            validated_read_with(&evidence, &[root], None, || {
                note_read(root, ["file"]);
                Ok(7)
            })?,
            Some(7)
        );
        assert_eq!(evidence.scans.load(Ordering::Relaxed), 2);
        assert_eq!(
            validated_read_with(&evidence, &[root], None, || {
                note_read(root, ["file"]);
                evidence.change();
                Ok(9)
            })?,
            None
        );
        assert_eq!(
            validated_read_with(&evidence, &[root], None, || panic!("unverifiable entries require a capture"))?,
            None::<()>
        );
        Ok(())
    }
    #[test]
    fn scoped_reads_cannot_silently_use_unobserved_paths() -> Result<()> {
        let evidence = Evidence::new();
        let root = Path::new("/observed");
        let paths = BTreeSet::from(["file".into()]);
        assert_eq!(
            validated_read_with(&evidence, &[root], Some(&paths), || {
                note_read(root, ["outside-even-if-absent"]);
                Ok(7)
            })?,
            None
        );
        assert_eq!(evidence.scans.load(Ordering::Relaxed), 1);
        Ok(())
    }
    #[test]
    fn nested_read_coverage_is_retained_by_its_outer_validation() -> Result<()> {
        let evidence = Evidence::new();
        let root = Path::new("/observed");
        let result = validated_read_with(&evidence, &[root], None, || {
            assert_eq!(
                validated_read_with(&evidence, &[root], None, || {
                    note_read(root, ["file"]);
                    Ok(5)
                })?,
                Some(5)
            );
            evidence.change();
            Ok(7)
        })?;
        assert_eq!(result, None);
        assert_eq!(evidence.scans.load(Ordering::Relaxed), 4);
        Ok(())
    }
}
