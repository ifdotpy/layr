// Native mode, device and link-count widths differ between Linux and macOS.
#![allow(clippy::useless_conversion)]
use layr_fs_core::{evidence::FType, *};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn sys<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|e| FsError::Io(e.into()))
}
/// Directory IO rooted in a retained descriptor. Paths never traverse a parent symlink.
pub struct DirectoryTree {
    root: File,
    serial: AtomicU64,
}
impl DirectoryTree {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_file(File::from(sys(fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ))?))
    }
    pub fn from_file(root: File) -> Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(FsError::Invalid("tree root is not a directory"));
        }
        Ok(Self { root, serial: AtomicU64::new(0) })
    }
    pub fn root_file(&self) -> &File {
        &self.root
    }
    /// Pin an entry for metadata APIs, including the symlink itself on macOS.
    pub fn open_metadata(&self, path: &RelDir) -> Result<File> {
        let RelDir::Path(path) = path else { return Ok(self.root.try_clone()?) };
        let (parent, name) = self.parent(path)?;
        Self::metadata_at(&parent, name)
    }
    fn metadata_at(parent: &File, name: &OsStr) -> Result<File> {
        #[cfg(target_os = "macos")]
        let flags = OFlags::RDONLY | OFlags::SYMLINK | OFlags::NONBLOCK | OFlags::CLOEXEC;
        #[cfg(not(target_os = "macos"))]
        let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(File::from(sys(fs::openat(&parent, name, flags, Mode::empty()))?))
    }
    pub fn open_directory(&self, dir: &RelDir) -> Result<File> {
        self.directory(dir.as_bytes())
    }
    fn directory(&self, dir: &[u8]) -> Result<File> {
        let mut current = self.root.try_clone()?;
        if !dir.is_empty() {
            RelPath::new(dir)?;
            for name in dir.split(|b| *b == b'/') {
                current = File::from(sys(fs::openat(
                    &current,
                    OsStr::from_bytes(name),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ))?);
            }
        }
        Ok(current)
    }
    fn parent<'a>(&self, path: &'a RelPath) -> Result<(File, &'a OsStr)> {
        let bytes = path.as_bytes();
        let (parent, name) = match bytes.iter().rposition(|b| *b == b'/') {
            Some(i) => (&bytes[..i], &bytes[i + 1..]),
            None => (b"".as_slice(), bytes),
        };
        Ok((self.directory(parent)?, OsStr::from_bytes(name)))
    }
    pub fn open_native_file(&self, path: &RelPath) -> Result<File> {
        let file = self.open_entry(path, OFlags::RDONLY)?;
        if !file.metadata()?.is_file() {
            return Err(FsError::Invalid("not a regular file"));
        }
        Ok(file)
    }
    fn open_entry(&self, path: &RelPath, flags: OFlags) -> Result<File> {
        let (parent, name) = self.parent(path)?;
        Ok(File::from(sys(fs::openat(
            &parent,
            name,
            flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ))?))
    }
    fn temporary_name(&self) -> Result<CString> {
        CString::new(format!(".layr-write-{}", self.serial.fetch_add(1, Ordering::Relaxed)))
            .map_err(|_| FsError::Invalid("invalid temporary name"))
    }
    fn apply_symlink(parent: &File, name: &OsStr, attributes: &EntryAttributes) -> Result<()> {
        if attributes.acl.is_some() {
            return Err(FsError::Invalid("native ACL needs a platform writer"));
        }
        set_symlink_xattrs(parent, name, &attributes.xattrs)?;
        sys(fs::chownat(
            parent,
            name,
            Some(rustix::process::Uid::from_raw(attributes.uid)),
            Some(rustix::process::Gid::from_raw(attributes.gid)),
            AtFlags::SYMLINK_NOFOLLOW,
        ))?;
        let timestamp = fs::Timespec {
            tv_sec: attributes.modified_ns.div_euclid(1_000_000_000),
            tv_nsec: attributes.modified_ns.rem_euclid(1_000_000_000) as _,
        };
        sys(fs::utimensat(
            parent,
            name,
            &fs::Timestamps { last_access: timestamp, last_modification: timestamp },
            AtFlags::SYMLINK_NOFOLLOW,
        ))
    }
    fn apply(file: &File, attributes: &EntryAttributes) -> Result<()> {
        if attributes.acl.is_some() {
            return Err(FsError::Invalid("native ACL needs a platform writer"));
        }
        let meta = file.metadata()?;
        if meta.uid() != attributes.uid || meta.gid() != attributes.gid {
            sys(fs::fchown(
                file,
                Some(rustix::process::Uid::from_raw(attributes.uid)),
                Some(rustix::process::Gid::from_raw(attributes.gid)),
            ))?;
        }
        let mode = Mode::from_raw_mode(attributes.mode.try_into().map_err(|_| FsError::Invalid("mode out of range"))?);
        // macOS requires owner-write permission even for descriptor xattr operations.
        sys(fs::fchmod(file, mode | Mode::WUSR))?;
        let synchronized: Result<()> = (|| {
            let current = xattrs(file)?;
            for (name, _) in current.iter().filter(|(name, _)| !attributes.xattrs.iter().any(|(wanted, _)| name == wanted)) {
                sys(fs::fremovexattr(file, OsStr::from_bytes(name)))?;
            }
            for (name, value) in &attributes.xattrs {
                sys(fs::fsetxattr(file, OsStr::from_bytes(name), value, fs::XattrFlags::empty()))?;
            }
            Ok(())
        })();
        let restored = sys(fs::fchmod(file, mode));
        synchronized?;
        restored?;
        let timestamp = fs::Timespec {
            tv_sec: attributes.modified_ns.div_euclid(1_000_000_000),
            tv_nsec: attributes.modified_ns.rem_euclid(1_000_000_000) as _,
        };
        sys(fs::futimens(file, &fs::Timestamps { last_access: timestamp, last_modification: timestamp }))
    }
    fn remove_at(parent: &File, name: &OsStr) -> Result<()> {
        let stat = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(e) => return Err(FsError::Io(e.into())),
        };
        if fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::Directory {
            let child = File::from(sys(fs::openat(
                parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?);
            let opened = child.metadata()?;
            if opened.dev() != u64::try_from(stat.st_dev).map_err(|_| FsError::Invalid("device out of range"))?
                || opened.ino() != stat.st_ino
            {
                return Err(FsError::Invalid("directory replaced during removal"));
            }
            sys(fs::fchmod(
                &child,
                Mode::from_raw_mode((opened.mode() | 0o700).try_into().map_err(|_| FsError::Invalid("mode out of range"))?),
            ))?;
            for entry in sys(fs::Dir::read_from(&child))? {
                let entry = sys(entry)?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    Self::remove_at(&child, OsStr::from_bytes(name))?;
                }
            }
            sys(fs::unlinkat(parent, name, AtFlags::REMOVEDIR))
        } else {
            sys(fs::unlinkat(parent, name, AtFlags::empty()))
        }
    }
    /// Removal is relative to an operation-owned parent and verifies the retained tree identity.
    pub fn remove_owned(parent: &File, name: &OsStr, expected: &File) -> Result<()> {
        let stat = sys(fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW))?;
        let opened = expected.metadata()?;
        if u64::try_from(stat.st_dev).map_err(|_| FsError::Invalid("device out of range"))? != opened.dev()
            || stat.st_ino != opened.ino()
        {
            return Err(FsError::Invalid("owned tree was replaced"));
        }
        Self::remove_at(parent, name)
    }
}
fn read_buffer(mut read: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>) -> Result<Vec<u8>> {
    let mut size = 256;
    loop {
        let mut bytes = vec![0; size];
        match read(&mut bytes) {
            Ok(count) => {
                bytes.truncate(count);
                return Ok(bytes);
            }
            Err(e) if e == rustix::io::Errno::RANGE && size < 1 << 26 => size *= 4,
            Err(e) => return Err(FsError::Io(e.into())),
        }
    }
}
fn attributes(
    mut list: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    mut get: impl FnMut(&OsStr, &mut [u8]) -> rustix::io::Result<usize>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let names = match read_buffer(&mut list) {
        Ok(names) => names,
        Err(FsError::Io(e))
            if e.raw_os_error() == Some(rustix::io::Errno::NOTSUP.raw_os_error())
                || e.raw_os_error() == Some(rustix::io::Errno::OPNOTSUPP.raw_os_error()) =>
        {
            return Ok(Vec::new())
        }
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for name in names.split(|b| *b == 0).filter(|name| !name.is_empty()) {
        out.push((name.to_vec(), read_buffer(|buffer| get(OsStr::from_bytes(name), buffer))?));
    }
    out.sort();
    Ok(out)
}
fn xattrs(file: &File) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    attributes(|buffer| fs::flistxattr(file, buffer), |name, buffer| fs::fgetxattr(file, name, buffer))
}
#[cfg(target_os = "linux")]
fn link_path(parent: &File, name: &OsStr) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    // /proc anchors pathname-only no-follow xattr APIs to the retained parent descriptor.
    std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name)
}
fn symlink_xattrs(parent: &File, name: &OsStr) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    #[cfg(target_os = "linux")]
    {
        let path = link_path(parent, name);
        attributes(|buffer| fs::llistxattr(&path, buffer), |name, buffer| fs::lgetxattr(&path, name, buffer))
    }
    #[cfg(target_os = "macos")]
    {
        // O_SYMLINK pins the link itself; O_NOFOLLOW must not be combined with it.
        let file = File::from(sys(fs::openat(parent, name, OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC, Mode::empty()))?);
        xattrs(&file)
    }
}
fn set_symlink_xattrs(parent: &File, name: &OsStr, attrs: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = link_path(parent, name);
        for (old, _) in symlink_xattrs(parent, name)?.iter().filter(|(old, _)| !attrs.iter().any(|(wanted, _)| old == wanted)) {
            sys(fs::lremovexattr(&path, OsStr::from_bytes(old)))?;
        }
        for (name, value) in attrs {
            sys(fs::lsetxattr(&path, OsStr::from_bytes(name), value, fs::XattrFlags::empty()))?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let file = File::from(sys(fs::openat(parent, name, OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC, Mode::empty()))?);
        for (old, _) in xattrs(&file)?.iter().filter(|(old, _)| !attrs.iter().any(|(wanted, _)| old == wanted)) {
            sys(fs::fremovexattr(&file, OsStr::from_bytes(old)))?;
        }
        for (name, value) in attrs {
            sys(fs::fsetxattr(&file, OsStr::from_bytes(name), value, fs::XattrFlags::empty()))?;
        }
    }
    Ok(())
}

impl TreeRead for DirectoryTree {
    fn link_domain(&self) -> Option<&LinkDomain> {
        static DOMAIN: std::sync::OnceLock<LinkDomain> = std::sync::OnceLock::new();
        Some(DOMAIN.get_or_init(LinkDomain::default))
    }
    fn kind(&self, path: &RelDir) -> Result<Option<FType>> {
        let stat = match path {
            RelDir::Root => sys(fs::fstat(&self.root))?,
            RelDir::Path(path) => {
                let (parent, name) = self.parent(path)?;
                match fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Ok(None),
                    Err(error) => return Err(FsError::Io(error.into())),
                }
            }
        };
        Ok(Some(match fs::FileType::from_raw_mode(stat.st_mode) {
            fs::FileType::Directory => FType::Dir,
            fs::FileType::RegularFile => FType::File,
            fs::FileType::Symlink => FType::Symlink,
            _ => FType::Special,
        }))
    }
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>> {
        let (stat, file) = match path {
            RelDir::Root => (sys(fs::fstat(&self.root))?, Some(self.root.try_clone()?)),
            RelDir::Path(path) => {
                let (parent, name) = match self.parent(path) {
                    Ok(pair) => pair,
                    Err(FsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error),
                };
                let stat = match fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(rustix::io::Errno::NOENT) => return Ok(None),
                    Err(e) => return Err(FsError::Io(e.into())),
                };
                let kind = fs::FileType::from_raw_mode(stat.st_mode);
                let file = if kind == fs::FileType::RegularFile || kind == fs::FileType::Directory {
                    Some(self.open_entry(path, OFlags::RDONLY)?)
                } else {
                    None
                };
                (stat, file)
            }
        };
        let kind = match fs::FileType::from_raw_mode(stat.st_mode) {
            fs::FileType::RegularFile => FType::File,
            fs::FileType::Directory => FType::Dir,
            fs::FileType::Symlink => FType::Symlink,
            _ => FType::Special,
        };
        let nanos = i64::try_from(stat.st_mtime_nsec).map_err(|_| FsError::Invalid("nanoseconds out of range"))?;
        let modified_ns = i64::from(stat.st_mtime)
            .checked_mul(1_000_000_000)
            .and_then(|n| n.checked_add(nanos))
            .ok_or(FsError::Invalid("timestamp out of range"))?;
        let size = u64::try_from(stat.st_size).map_err(|_| FsError::Invalid("negative entry size"))?;
        let attributes = EntryAttributes {
            acl: None,
            mode: u32::from(stat.st_mode) & 0o7777,
            uid: stat.st_uid,
            gid: stat.st_gid,
            modified_ns,
            stored_time: Some(modified_ns),
            xattrs: match file {
                Some(file) => xattrs(&file)?,
                None if kind == FType::Symlink => {
                    let RelDir::Path(path) = path else {
                        return Err(FsError::Invalid("root cannot be a symbolic link"));
                    };
                    let (parent, name) = self.parent(path)?;
                    symlink_xattrs(&parent, name)?
                }
                None => Vec::new(),
            },
        };
        Ok(Some(EntryMetadata {
            kind,
            size,
            attributes,
            link: (kind != FType::Dir).then_some(LinkIdentity {
                device: u64::try_from(stat.st_dev).map_err(|_| FsError::Invalid("device out of range"))?,
                inode: stat.st_ino,
                count: u64::from(stat.st_nlink),
            }),
        }))
    }
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
        let directory = self.directory(dir.as_bytes())?;
        let mut names = Vec::new();
        for entry in sys(fs::Dir::read_from(&directory))? {
            let entry = sys(entry)?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(RelPath::new(name.to_vec())?);
            }
        }
        names.sort();
        for name in names {
            visit(&name)?;
        }
        Ok(())
    }
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
        Ok(Box::new(self.open_native_file(path)?))
    }
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>> {
        let (parent, name) = self.parent(path)?;
        Ok(sys(fs::readlinkat(&parent, name, Vec::new()))?.into_bytes())
    }
}
impl DirectoryTree {
    pub fn put_with_metadata(
        &self,
        path: &RelPath,
        data: EntryData<'_>,
        attributes: &EntryAttributes,
        apply: &dyn Fn(&File) -> Result<()>,
    ) -> Result<()> {
        let (parent, name) = self.parent(path)?;
        match data {
            EntryData::Directory => {
                sys(fs::mkdirat(&parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR))?;
                let directory = self.open_entry(path, OFlags::RDONLY)?;
                Self::apply(&directory, attributes)?;
                return apply(&directory);
            }
            EntryData::Symlink(target) => {
                let temporary = loop {
                    let name = self.temporary_name()?;
                    match fs::symlinkat(OsStr::from_bytes(target), &parent, name.as_c_str()) {
                        Ok(()) => break name,
                        Err(rustix::io::Errno::EXIST) => continue,
                        Err(error) => return Err(FsError::Io(error.into())),
                    }
                };
                let result = (|| {
                    let temporary_name = OsStr::from_bytes(temporary.to_bytes());
                    Self::apply_symlink(&parent, temporary_name, attributes)?;
                    apply(&Self::metadata_at(&parent, temporary_name)?)?;
                    sys(fs::renameat(&parent, temporary.as_c_str(), &parent, name))
                })();
                if result.is_err() {
                    let _ = fs::unlinkat(&parent, temporary.as_c_str(), AtFlags::empty());
                }
                return result;
            }
            EntryData::HardLink(source) => {
                let (source_parent, source_name) = self.parent(source)?;
                let stat = sys(fs::statat(&source_parent, source_name, AtFlags::SYMLINK_NOFOLLOW))?;
                if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile {
                    return Err(FsError::Invalid("hard-link source is not a regular file"));
                }
                sys(fs::linkat(&source_parent, source_name, &parent, name, AtFlags::empty()))?;
                return Ok(());
            }
            EntryData::File(reader) => {
                let temporary = loop {
                    let candidate = self.temporary_name()?;
                    match fs::openat(
                        &parent,
                        candidate.as_c_str(),
                        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::RUSR | Mode::WUSR,
                    ) {
                        Ok(fd) => break (candidate, File::from(fd)),
                        Err(rustix::io::Errno::EXIST) => continue,
                        Err(e) => return Err(FsError::Io(e.into())),
                    }
                };
                let result = (|| {
                    let mut output = &temporary.1;
                    std::io::copy(reader, &mut output)?;
                    Self::apply(&temporary.1, attributes)?;
                    apply(&temporary.1)?;
                    sys(fs::renameat(&parent, temporary.0.as_c_str(), &parent, name))
                })();
                if result.is_err() {
                    let _ = fs::unlinkat(&parent, temporary.0.as_c_str(), AtFlags::empty());
                }
                return result;
            }
        }
    }
    pub fn set_metadata_with(
        &self,
        path: &RelDir,
        attributes: &EntryAttributes,
        apply: &dyn Fn(&File) -> Result<()>,
    ) -> Result<()> {
        match path {
            RelDir::Root => {
                Self::apply(&self.root, attributes)?;
                apply(&self.root)
            }
            RelDir::Path(path) => {
                let metadata = self.metadata(&RelDir::Path(path.clone()))?.ok_or(FsError::Invalid("entry absent"))?;
                if metadata.kind == FType::Symlink {
                    let (parent, name) = self.parent(path)?;
                    Self::apply_symlink(&parent, name, attributes)?;
                    apply(&Self::metadata_at(&parent, name)?)
                } else {
                    let file = self.open_entry(path, OFlags::RDONLY)?;
                    Self::apply(&file, attributes)?;
                    apply(&file)
                }
            }
        }
    }
}
impl TreeWrite for DirectoryTree {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> Result<()> {
        self.put_with_metadata(path, data, attributes, &|_| Ok(()))
    }
    fn rename_new(&self, source: &RelPath, destination: &RelPath) -> Result<()> {
        let (from, source) = self.parent(source)?;
        let (to, destination) = self.parent(destination)?;
        sys(fs::renameat_with(&from, source, &to, destination, fs::RenameFlags::NOREPLACE))
    }
    fn remove_entry(&self, path: &RelPath) -> Result<()> {
        let (parent, name) = self.parent(path)?;
        Self::remove_at(&parent, name)
    }
    fn remove_empty_dir(&self, path: &RelPath) -> Result<bool> {
        let (parent, name) = self.parent(path)?;
        match fs::unlinkat(&parent, name, AtFlags::REMOVEDIR) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(true),
            Err(rustix::io::Errno::NOTEMPTY) => Ok(false),
            Err(error) => Err(FsError::Io(error.into())),
        }
    }
    fn overwrite_file(&self, path: &RelPath, source: &mut dyn std::io::Read, modified_ns: i64) -> Result<Attempt<()>> {
        let mut file = match self.open_entry(path, OFlags::WRONLY) {
            Ok(file) => file,
            Err(FsError::Io(error)) => return Ok(Attempt::Unavailable(error)),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(FsError::Invalid("not a regular file"));
        }
        file.set_len(0)?;
        std::io::copy(source, &mut file)?;
        set_time(&file, modified_ns)?;
        Ok(Attempt::Applied(()))
    }
    fn write_range(&self, path: &RelPath, offset: u64, bytes: &[u8], modified_ns: i64) -> Result<()> {
        use std::os::unix::fs::FileExt;
        let file = self.open_entry(path, OFlags::WRONLY)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(FsError::Invalid("not a regular file"));
        }
        if offset.checked_add(bytes.len() as u64).is_none_or(|end| end > metadata.len()) {
            return Err(FsError::Invalid("range exceeds file"));
        }
        file.write_all_at(bytes, offset)?;
        set_time(&file, modified_ns)
    }
    fn set_metadata(&self, path: &RelDir, attributes: &EntryAttributes) -> Result<()> {
        self.set_metadata_with(path, attributes, &|_| Ok(()))
    }
}

fn set_time(file: &File, modified_ns: i64) -> Result<()> {
    sys(fs::futimens(
        file,
        &fs::Timestamps {
            last_access: fs::Timespec { tv_sec: 0, tv_nsec: fs::UTIME_OMIT },
            last_modification: fs::Timespec {
                tv_sec: modified_ns.div_euclid(1_000_000_000),
                tv_nsec: modified_ns.rem_euclid(1_000_000_000) as _,
            },
        },
    ))
}
