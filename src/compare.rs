//! Pure operations on comparison evidence.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FType {
    File,
    Dir,
    Symlink,
    Special,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub ftype: FType,
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        self.ftype == FType::File
    }
    pub fn is_dir(&self) -> bool {
        self.ftype == FType::Dir
    }
    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }
    /// The git mode of this entry: 100644, 100755, 120000 or 040000.
    pub fn git_mode(&self) -> u32 {
        match self.ftype {
            FType::Symlink => 0o120000,
            FType::Dir => 0o040000,
            _ if self.executable() => 0o100755,
            _ => 0o100644,
        }
    }
}

/// The end of an extent; btrfs writes without a known length extend through EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum RangeEnd {
    Offset(u64),
    Eof,
}

/// Sorted, disjoint extents with nonempty intervals. Construction checks arithmetic and merges overlap.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<(u64, RangeEnd)>", into = "Vec<(u64, RangeEnd)>")]
pub struct ChangedRanges(Vec<(u64, RangeEnd)>);

impl TryFrom<Vec<(u64, RangeEnd)>> for ChangedRanges {
    type Error = &'static str;
    fn try_from(mut ranges: Vec<(u64, RangeEnd)>) -> Result<Self, Self::Error> {
        if ranges.iter().any(|(start, end)| *end <= RangeEnd::Offset(*start)) {
            return Err("empty or reversed byte range");
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u64, RangeEnd)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if RangeEnd::Offset(start) <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Ok(Self(merged))
    }
}
impl From<ChangedRanges> for Vec<(u64, RangeEnd)> {
    fn from(ranges: ChangedRanges) -> Self {
        ranges.0
    }
}
impl ChangedRanges {
    /// Decode the native offset/length format at the driver boundary.
    pub fn from_native(ranges: &[(u64, u64)]) -> Result<Self, &'static str> {
        ranges
            .iter()
            .filter(|(_, len)| *len != 0)
            .map(|(start, len)| {
                let end = if *len == u64::MAX {
                    RangeEnd::Eof
                } else {
                    RangeEnd::Offset(start.checked_add(*len).ok_or("byte range overflow")?)
                };
                Ok::<_, &'static str>((*start, end))
            })
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
    }
    pub fn bounded(&self, size: u64) -> Vec<(u64, u64)> {
        self.0
            .iter()
            .filter(|(start, _)| *start < size)
            .map(|(start, end)| {
                let end = match end {
                    RangeEnd::Offset(end) => (*end).min(size),
                    RangeEnd::Eof => size,
                };
                (*start, end - start)
            })
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DataEvidence {
    Unknown,
    Equal,
    ChangedRanges(ChangedRanges),
}
impl DataEvidence {
    #[cfg(any(target_os = "macos", feature = "overlayfs", test))]
    pub fn from_native(ranges: &[(u64, u64)], old_size: u64, new_size: u64) -> Result<Self, &'static str> {
        Ok(Self::from_ranges(ChangedRanges::from_native(ranges)?, old_size, new_size))
    }
    pub fn from_ranges(ranges: ChangedRanges, old_size: u64, new_size: u64) -> Self {
        if ranges.is_empty() && old_size == new_size {
            Self::Equal
        } else {
            Self::ChangedRanges(ranges)
        }
    }
}

/// Observed changes carry every side required by their kind.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EntryChange {
    Add { path: String, new: Meta },
    Remove { path: String, old: Meta },
    Modify { path: String, old: Meta, new: Meta, data: DataEvidence },
    ReplaceType { path: String, old: Meta, new: Meta },
    Rename { path: String, old_path: String, old: Meta, new: Meta, data: DataEvidence },
}
impl EntryChange {
    /// Convert optional observations once, at the source boundary. Two absent sides are no candidate.
    pub fn observed(path: String, old: Option<Meta>, new: Option<Meta>, data: DataEvidence) -> Option<Self> {
        Some(match (old, new) {
            (None, None) => return None,
            (None, Some(new)) => Self::Add { path, new },
            (Some(old), None) => Self::Remove { path, old },
            (Some(old), Some(new)) if old.ftype != new.ftype => Self::ReplaceType { path, old, new },
            (Some(old), Some(new)) => Self::Modify { path, old, new, data },
        })
    }
    pub fn src(&self) -> &str {
        self.old_path().unwrap_or(self.path())
    }
    pub fn is_rename(&self) -> bool {
        matches!(self, Self::Rename { .. })
    }
    pub fn path(&self) -> &String {
        match self {
            Self::Add { path, .. }
            | Self::Remove { path, .. }
            | Self::Modify { path, .. }
            | Self::ReplaceType { path, .. }
            | Self::Rename { path, .. } => path,
        }
    }
    pub fn old(&self) -> Option<&Meta> {
        match self {
            Self::Add { .. } => None,
            Self::Remove { old, .. } | Self::Modify { old, .. } | Self::ReplaceType { old, .. } | Self::Rename { old, .. } => {
                Some(old)
            }
        }
    }
    pub fn new(&self) -> Option<&Meta> {
        match self {
            Self::Remove { .. } => None,
            Self::Add { new, .. } | Self::Modify { new, .. } | Self::ReplaceType { new, .. } | Self::Rename { new, .. } => {
                Some(new)
            }
        }
    }
    pub fn old_path(&self) -> Option<&String> {
        match self {
            Self::Rename { old_path, .. } => Some(old_path),
            _ => None,
        }
    }
    pub fn data(&self) -> &DataEvidence {
        match self {
            Self::Modify { data, .. } | Self::Rename { data, .. } => data,
            _ => &DataEvidence::Unknown,
        }
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    pub fn set_data(&mut self, evidence: DataEvidence) {
        match self {
            Self::Modify { data, .. } | Self::Rename { data, .. } => *data = evidence,
            _ => {}
        }
    }
}

/// Pair only a removed source and an added destination of the same type.
pub fn pair_rename(changes: &mut std::collections::BTreeMap<String, EntryChange>, src: &str, dst: &str, data: DataEvidence) {
    if let (Some(EntryChange::Remove { old, .. }), Some(EntryChange::Add { new, .. })) = (changes.get(src), changes.get(dst)) {
        if old.ftype == new.ftype {
            let renamed =
                EntryChange::Rename { path: dst.to_owned(), old_path: src.to_owned(), old: old.clone(), new: new.clone(), data };
            changes.remove(src);
            changes.insert(dst.to_owned(), renamed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_merge_clip_and_preserve_eof() {
        let ranges = ChangedRanges::from_native(&[(8, 4), (0, 5), (5, 4), (20, u64::MAX)]).unwrap();
        assert_eq!(ranges.0, [(0, RangeEnd::Offset(12)), (20, RangeEnd::Eof)]);
        assert_eq!(ranges.bounded(25), [(0, 12), (20, 5)]);
        assert_eq!(ranges.bounded(10), [(0, 10)]);
        assert!(ChangedRanges::from_native(&[(u64::MAX - 1, 3)]).is_err());
        assert!(ChangedRanges::try_from(vec![(5, RangeEnd::Offset(4))]).is_err());
    }
    #[test]
    fn cache_evidence_round_trips_and_rejects_invalid_ranges() {
        for evidence in
            [DataEvidence::Unknown, DataEvidence::Equal, DataEvidence::from_native(&[(0, 4), (8, u64::MAX)], 16, 16).unwrap()]
        {
            let encoded = serde_json::to_vec(&evidence).unwrap();
            assert_eq!(serde_json::from_slice::<DataEvidence>(&encoded).unwrap(), evidence);
        }
        for malformed in [r#"[[5,{"Offset":4}]]"#, r#"[[5,{"Offset":5}]]"#] {
            assert!(serde_json::from_str::<ChangedRanges>(malformed).is_err());
        }
    }

    fn meta(ftype: FType) -> Meta {
        Meta { ftype, mode: 0o644, size: 1, ino: 0, uid: 0, gid: 0, mtime: 0 }
    }
    #[test]
    fn observations_and_pairing_require_present_sides() {
        let file = meta(FType::File);
        let dir = meta(FType::Dir);
        let observed = |old, new| EntryChange::observed("p".into(), old, new, DataEvidence::Unknown);
        assert!(observed(None, None).is_none());
        assert!(matches!(observed(None, Some(file.clone())), Some(EntryChange::Add { .. })));
        assert!(matches!(observed(Some(file.clone()), None), Some(EntryChange::Remove { .. })));
        assert!(matches!(observed(Some(file.clone()), Some(file.clone())), Some(EntryChange::Modify { .. })));
        assert!(matches!(observed(Some(file.clone()), Some(dir.clone())), Some(EntryChange::ReplaceType { .. })));
        let mut changes = std::collections::BTreeMap::from([
            ("src".into(), EntryChange::Remove { path: "src".into(), old: file.clone() }),
            ("dst".into(), EntryChange::Add { path: "dst".into(), new: dir }),
        ]);
        pair_rename(&mut changes, "src", "dst", DataEvidence::Unknown);
        assert_eq!(changes.len(), 2, "different types cannot pair");
        changes.insert("dst".into(), EntryChange::Add { path: "dst".into(), new: file });
        pair_rename(&mut changes, "src", "dst", DataEvidence::Equal);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes["dst"], EntryChange::Rename { .. }));
        assert_eq!(changes["dst"].old_path().map(String::as_str), Some("src"));
        assert_eq!(changes["dst"].data(), &DataEvidence::Equal);
    }
    #[test]
    fn empty_extents_require_equal_lengths() {
        assert_eq!(DataEvidence::from_native(&[], 1, 1).unwrap(), DataEvidence::Equal);
        assert!(matches!(DataEvidence::from_native(&[], 1, 2).unwrap(), DataEvidence::ChangedRanges(_)));
    }
}
