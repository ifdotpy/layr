//! Writable merge output. Native policy stays in its existing helpers; core writes use handles.
use crate::fsutil::{self, Src};
use anyhow::{bail, Context, Result};
use layr_fs_core::{evidence::FType, EntryAttributes, EntryData, RelDir, RelPath, TreeRead, TreeWrite};
use std::io::Read;
use std::path::{Path, PathBuf};

pub enum WriteSrc<'a> {
    Native(&'a Path),
    // Reference outputs are exercised in contract tests; platform adapters migrate next.
    #[cfg_attr(not(test), allow(dead_code))]
    Core {
        tree: &'a dyn TreeWrite,
        modified_ns: i64,
    },
}
impl Src for WriteSrc<'_> {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        match self {
            Self::Native(path) => path.at(rel),
            Self::Core { .. } => bail!("core merge outputs have no native path"),
        }
    }
    fn folder(&self) -> Option<&Path> {
        match self {
            Self::Native(path) => Some(path),
            Self::Core { .. } => None,
        }
    }
    fn core_reader(&self) -> Option<&dyn TreeRead> {
        match self {
            Self::Native(_) => None,
            Self::Core { tree, .. } => Some(*tree),
        }
    }
}
impl WriteSrc<'_> {
    pub fn make_parents(&self, rel: &str) -> Result<()> {
        match self {
            Self::Native(root) => fsutil::make_parents(root, rel, None),
            Self::Core { tree, modified_ns } => {
                fsutil::check_rel(rel)?;
                for (i, _) in rel.match_indices('/') {
                    let path = RelPath::new(rel[..i].as_bytes())?;
                    match tree.metadata(&RelDir::Path(path.clone()))? {
                        Some(meta) if meta.kind == FType::Dir => {}
                        Some(_) => bail!("parent is not a directory"),
                        None => tree.put(&path, EntryData::Directory, &fresh_attributes(0o755, *modified_ns))?,
                    }
                }
                Ok(())
            }
        }
    }
    pub fn mkdirs(&self, rel: &str) -> Result<()> {
        match self {
            Self::Native(root) => Ok(std::fs::create_dir_all(fsutil::safe_join(root, rel)?)?),
            Self::Core { .. } => self.make_parents(&format!("{rel}/x")),
        }
    }
    pub fn remove(&self, rel: &str) -> Result<()> {
        match self {
            Self::Native(root) => fsutil::remove_entry(root, rel),
            Self::Core { tree, .. } => Ok(tree.remove_entry(&RelPath::new(rel.as_bytes())?)?),
        }
    }
    pub fn remove_empty_dir(&self, rel: &str) -> Result<bool> {
        match self {
            Self::Native(root) => Ok(std::fs::remove_dir(fsutil::safe_join(root, rel)?).is_ok()),
            Self::Core { tree, .. } => Ok(tree.remove_empty_dir(&RelPath::new(rel.as_bytes())?)?),
        }
    }
    pub fn rename(&self, source: &str, destination: &str) -> Result<()> {
        self.make_parents(destination)?;
        match self {
            Self::Native(root) => Ok(std::fs::rename(fsutil::safe_join(root, source)?, fsutil::safe_join(root, destination)?)?),
            Self::Core { tree, .. } => {
                Ok(tree.rename_new(&RelPath::new(source.as_bytes())?, &RelPath::new(destination.as_bytes())?)?)
            }
        }
    }
    pub fn copy_from(&self, source: &dyn Src, from: &str, to: &str) -> Result<()> {
        let tree = match self {
            Self::Native(root) => return fsutil::copy_entry(source, from, root, to, None),
            Self::Core { tree, .. } => *tree,
        };
        self.make_parents(to)?;
        let from_path = RelPath::new(from.as_bytes())?;
        let to_path = RelPath::new(to.as_bytes())?;
        let meta = match source.core_reader() {
            Some(reader) => layr_fs_common::required(reader, &RelDir::Path(from_path))?,
            None => {
                use std::os::unix::fs::MetadataExt;
                let path = source.at(from)?;
                let metadata = std::fs::symlink_metadata(&path)?;
                let old = fsutil::meta_of(&metadata);
                let stamp = layr_objects::times::ns_of(&metadata);
                layr_fs_core::EntryMetadata {
                    kind: old.ftype,
                    size: old.size,
                    link: None,
                    attributes: EntryAttributes {
                        acl: None,
                        mode: old.mode,
                        uid: metadata.uid(),
                        gid: metadata.gid(),
                        modified_ns: stamp,
                        stored_time: Some(stamp),
                        xattrs: layr_objects::fsx::xattrs(&path)?,
                    },
                }
            }
        };
        if tree.metadata(&RelDir::Path(to_path.clone()))?.is_some_and(|m| m.kind != meta.kind) {
            tree.remove_entry(&to_path)?;
        }
        match meta.kind {
            FType::Dir => {
                if tree.metadata(&RelDir::Path(to_path.clone()))?.is_none() {
                    tree.put(&to_path, EntryData::Directory, &meta.attributes)?;
                } else {
                    tree.set_metadata(&RelDir::Path(to_path), &meta.attributes)?;
                }
            }
            FType::File => {
                let mut input = fsutil::open_source(source, from, &layr_fs_common::observation(&meta))?;
                if tree
                    .metadata(&RelDir::Path(to_path.clone()))?
                    .is_some_and(|meta| meta.link.is_some_and(|link| link.count == 1))
                {
                    match tree.overwrite_file(&to_path, &mut input, meta.attributes.modified_ns)? {
                        layr_fs_core::Attempt::Applied(()) => tree.set_metadata(&RelDir::Path(to_path), &meta.attributes)?,
                        layr_fs_core::Attempt::Unavailable(_) => {
                            tree.put(&to_path, EntryData::File(&mut input), &meta.attributes)?
                        }
                    }
                } else {
                    tree.put(&to_path, EntryData::File(&mut input), &meta.attributes)?;
                }
            }
            FType::Symlink => {
                tree.put(&to_path, EntryData::Symlink(&fsutil::read_link_bytes(source, from)?), &meta.attributes)?
            }
            FType::Special => bail!("cannot copy a special file"),
        }
        Ok(())
    }
    pub fn set_mode(&self, rel: &str, mode: u32) -> Result<()> {
        match self {
            Self::Native(root) => fsutil::set_mode(root, rel, mode),
            Self::Core { tree, .. } => {
                let at = RelDir::Path(RelPath::new(rel.as_bytes())?);
                let mut attributes = layr_fs_common::required(*tree, &at)?.attributes;
                attributes.mode = mode;
                Ok(tree.set_metadata(&at, &attributes)?)
            }
        }
    }
    pub fn write_from(&self, rel: &str, source: &mut dyn Read, mode: u32) -> Result<()> {
        self.make_parents(rel)?;
        match self {
            Self::Native(root) => fsutil::write_file_from(root, rel, source, mode, None),
            Self::Core { tree, modified_ns } => {
                let path = RelPath::new(rel.as_bytes())?;
                if tree.metadata(&RelDir::Path(path.clone()))?.is_some_and(|meta| meta.kind == FType::Dir) {
                    tree.remove_entry(&path)?;
                }
                Ok(tree.put(&path, EntryData::File(source), &fresh_attributes(mode, *modified_ns))?)
            }
        }
    }
    pub fn overwrite(&self, rel: &str, source: &mut dyn Read) -> Result<()> {
        match self {
            Self::Native(root) => {
                let mut file = crate::tree::Tree::open(root)?.open_write(rel)?.context("merge output is not a regular file")?;
                file.set_len(0)?;
                std::io::copy(source, &mut file)?;
                Ok(())
            }
            Self::Core { tree, modified_ns } => {
                match tree.overwrite_file(&RelPath::new(rel.as_bytes())?, source, *modified_ns)? {
                    layr_fs_core::Attempt::Applied(()) => Ok(()),
                    layr_fs_core::Attempt::Unavailable(error) => Err(error.into()),
                }
            }
        }
    }
    pub fn patch(
        &self,
        rel: &str,
        source: &mut dyn layr_fs_core::ReadSeek,
        native_source: Option<&std::fs::File>,
        ranges: &[(u64, u64)],
    ) -> Result<()> {
        use std::io::SeekFrom;
        use std::os::unix::fs::FileExt;
        enum Target<'a> {
            Native(std::fs::File),
            Core(&'a dyn TreeWrite, i64),
        }
        let output = match self {
            Self::Native(root) => {
                Target::Native(crate::tree::Tree::open(root)?.open_write(rel)?.context("merge output is not a regular file")?)
            }
            Self::Core { tree, modified_ns } => Target::Core(*tree, *modified_ns),
        };
        let path = RelPath::new(rel.as_bytes())?;
        let mut buffer = vec![0; 1 << 20];
        for &(offset, length) in ranges {
            if let (Some(source), Target::Native(target)) = (native_source, &output) {
                // Preserve the legacy policy: reflink errors select byte copying.
                if crate::fs::copy_range(source, offset, length, target, offset).is_ok() {
                    continue;
                }
            }
            source.seek(SeekFrom::Start(offset))?;
            let mut done = 0;
            while done < length {
                let count = (length - done).min(buffer.len() as u64) as usize;
                source.read_exact(&mut buffer[..count])?;
                match &output {
                    Target::Native(target) => target.write_all_at(&buffer[..count], offset + done)?,
                    Target::Core(tree, modified_ns) => tree.write_range(&path, offset + done, &buffer[..count], *modified_ns)?,
                }
                done += count as u64;
            }
        }
        if let Target::Native(file) = output {
            file.sync_all()?;
        }
        Ok(())
    }
}

fn fresh_attributes(mode: u32, modified_ns: i64) -> EntryAttributes {
    EntryAttributes {
        acl: None,
        mode,
        modified_ns,
        stored_time: Some(modified_ns),
        xattrs: Vec::new(),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    }
}
