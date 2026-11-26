#[cfg(all(target_os = "linux", not(test)))]
use rustix::fs::ioctl_ficlone;
use rustix::fs::{seek, SeekFrom};
use std::{
    fs::File,
    io,
    os::unix::fs::{FileExt, MetadataExt},
};
#[cfg(all(target_os = "linux", test))]
use tests::ioctl_ficlone;

/// Replace file content, preserving the destination inode and both descriptor positions.
/// A shrinking destination is truncated before attempting a native clone. This operation
/// owns its fallback and can return an error after changing the destination.
pub fn copy_contents(source: &File, destination: &File) -> io::Result<()> {
    let from = source.metadata()?;
    let to = destination.metadata()?;
    if !from.is_file() || !to.is_file() || (from.dev(), from.ino()) == (to.dev(), to.ino()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "content copy requires distinct regular files"));
    }
    let size = from.len();
    #[cfg(target_os = "linux")]
    let shrinking = size < to.len();
    #[cfg(target_os = "linux")]
    if !shrinking && matches!(clone_contents(source, destination)?, layr_fs_core::Attempt::Applied(())) {
        return Ok(());
    }

    let offset = seek(source, SeekFrom::Current(0))?;
    destination.set_len(0)?;
    #[cfg(target_os = "linux")]
    if shrinking && size != 0 && matches!(clone_whole_file(source, destination)?, layr_fs_core::Attempt::Applied(())) {
        return Ok(());
    }
    copy_sparse(source, destination, size, offset)
}

#[cfg(target_os = "linux")]
pub fn clone_contents(source: &File, destination: &File) -> io::Result<layr_fs_core::Attempt<()>> {
    // FICLONE can retain an old tail when the source ends on a block boundary.
    // Decline before mutation: the index caller needs an atomic replacement on
    // fallback, and truncating after the clone could fail after changing its bytes.
    if source.metadata()?.len() < destination.metadata()?.len() {
        return Ok(layr_fs_core::Attempt::Unavailable(rustix::io::Errno::OPNOTSUPP.into()));
    }
    clone_whole_file(source, destination)
}

#[cfg(target_os = "linux")]
fn clone_whole_file(source: &File, destination: &File) -> io::Result<layr_fs_core::Attempt<()>> {
    match ioctl_ficlone(destination, source) {
        Ok(()) => Ok(layr_fs_core::Attempt::Applied(())),
        Err(error)
            if matches!(
                error,
                rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::XDEV | rustix::io::Errno::INVAL | rustix::io::Errno::NOTTY
            ) =>
        {
            Ok(layr_fs_core::Attempt::Unavailable(error.into()))
        }
        Err(error) => Err(error.into()),
    }
}

struct Position<'a> {
    file: &'a File,
    offset: u64,
}
impl Drop for Position<'_> {
    fn drop(&mut self) {
        let _ = seek(self.file, SeekFrom::Start(self.offset));
    }
}
fn copy_sparse(source: &File, destination: &File, size: u64, offset: u64) -> io::Result<()> {
    let position = Position { file: source, offset };
    destination.set_len(size)?;
    let result = (|| -> io::Result<()> {
        let mut at = 0;
        let mut buffer = vec![0; 1 << 20];
        while at < size {
            let data = match seek(source, SeekFrom::Data(at)) {
                Ok(offset) => offset,
                Err(rustix::io::Errno::NXIO) => break,
                Err(error) if sparse_unavailable(error) => return copy_bytes(source, destination, 0, size, &mut buffer),
                Err(error) => return Err(error.into()),
            };
            if data >= size {
                break;
            }
            let end = match seek(source, SeekFrom::Hole(data)) {
                Ok(offset) => offset.min(size),
                // The file shrank below `data` after SEEK_DATA: a writer truncated it during the copy. Stop as
                // SEEK_DATA does; the caller's settle check sees the change. Returned, it failed a save while a
                // process rewrote a file (save-unsettled on overlayfs-ext4, "No such device or address").
                Err(rustix::io::Errno::NXIO) => break,
                Err(error) if sparse_unavailable(error) => return copy_bytes(source, destination, 0, size, &mut buffer),
                Err(error) => return Err(error.into()),
            };
            if end <= data {
                return Err(io::Error::other("invalid sparse extent"));
            }
            copy_bytes(source, destination, data, end, &mut buffer)?;
            at = end;
        }
        Ok(())
    })();
    let restored = seek(position.file, SeekFrom::Start(position.offset)).map(|_| ()).map_err(io::Error::from);
    if restored.is_ok() {
        std::mem::forget(position);
    }
    result.and(restored)
}
fn sparse_unavailable(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS)
}
fn copy_bytes(source: &File, destination: &File, mut at: u64, end: u64, buffer: &mut [u8]) -> io::Result<()> {
    while at < end {
        let length =
            usize::try_from((end - at).min(buffer.len() as u64)).map_err(|_| io::Error::other("copy length overflow"))?;
        let count = source.read_at(&mut buffer[..length], at)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source changed during content copy"));
        }
        destination.write_all_at(&buffer[..count], at)?;
        at += count as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Read, Seek, Write},
        os::unix::fs::OpenOptionsExt,
    };
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> io::Result<Self> {
            let root = std::env::temp_dir().join(format!(
                "layr-copy-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            fs::create_dir(&root)?;
            Ok(Self(root))
        }
        fn file(&self, name: &str) -> io::Result<File> {
            fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(self.0.join(name))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn content_copy_shrinks_at_boundaries_preserving_inode_aliases_and_positions() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        let mut destination = fixture.file("destination")?;
        fs::hard_link(fixture.0.join("destination"), fixture.0.join("alias"))?;
        let inode = destination.metadata()?.ino();
        for size in [0, 32767, 32768, 32769, 65536, 131072, 1048576] {
            let expected = vec![b'a'; size];
            source.set_len(0)?;
            source.write_all_at(&expected, 0)?;
            let old = vec![b'b'; size + 237];
            destination.write_all_at(&old, 0)?;
            destination.set_len(old.len() as u64)?;
            source.seek(io::SeekFrom::Start(3))?;
            destination.seek(io::SeekFrom::Start(size as u64 + 17))?;
            #[cfg(target_os = "linux")]
            {
                let before = destination.metadata()?;
                assert!(matches!(
                    clone_contents(&source, &destination)?,
                    layr_fs_core::Attempt::Unavailable(error) if error.raw_os_error() == Some(rustix::io::Errno::OPNOTSUPP.raw_os_error())
                ));
                assert_eq!(fs::read(fixture.0.join("alias"))?, old);
                let after = destination.metadata()?;
                assert_eq!((after.mtime(), after.mtime_nsec()), (before.mtime(), before.mtime_nsec()));
                assert_eq!(source.stream_position()?, 3);
                assert_eq!(destination.stream_position()?, size as u64 + 17);
            }
            copy_contents(&source, &destination)?;
            assert_eq!(source.stream_position()?, 3);
            assert_eq!(destination.stream_position()?, size as u64 + 17);
            assert_eq!(destination.metadata()?.ino(), inode);
            assert_eq!(destination.metadata()?.len(), size as u64);
            assert_eq!(fs::read(fixture.0.join("alias"))?, expected);
            assert_eq!(fs::read(fixture.0.join("source"))?, expected);
        }
        Ok(())
    }
    #[test]
    fn content_copy_replaces_equal_and_shorter_destinations() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        let mut destination = fixture.file("destination")?;
        let inode = destination.metadata()?.ino();
        let expected = vec![b'a'; 32768];
        source.write_all_at(&expected, 0)?;
        #[cfg(target_os = "linux")]
        let cloning_supported = matches!(clone_contents(&source, &fixture.file("probe")?)?, layr_fs_core::Attempt::Applied(()));
        for old_size in [1usize, 32768] {
            destination.set_len(old_size as u64)?;
            destination.write_all_at(&vec![b'b'; old_size], 0)?;
            source.seek(io::SeekFrom::Start(3))?;
            destination.seek(io::SeekFrom::Start(17))?;
            #[cfg(target_os = "linux")]
            if cloning_supported {
                assert!(matches!(clone_contents(&source, &destination)?, layr_fs_core::Attempt::Applied(())));
                assert_eq!(fs::read(fixture.0.join("destination"))?, expected);
            }
            copy_contents(&source, &destination)?;
            assert_eq!(fs::read(fixture.0.join("destination"))?, expected);
            assert_eq!(destination.metadata()?.ino(), inode);
            assert_eq!(source.stream_position()?, 3);
            assert_eq!(destination.stream_position()?, 17);
        }
        Ok(())
    }
    #[test]
    fn sparse_copy_preserves_inode_aliases_and_positions() -> io::Result<()> {
        check_sparse_copy(copy_contents)
    }
    #[cfg(target_os = "linux")]
    std::thread_local! {
        static FICLONE_ERROR: std::cell::Cell<Option<rustix::io::Errno>> = const { std::cell::Cell::new(None) };
    }

    #[cfg(target_os = "linux")]
    pub(super) fn ioctl_ficlone(destination: &File, source: &File) -> rustix::io::Result<()> {
        FICLONE_ERROR.take().map_or_else(|| rustix::fs::ioctl_ficlone(destination, source), Err)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shrinking_copy_falls_back_when_reflink_is_unavailable() -> io::Result<()> {
        for error in [rustix::io::Errno::OPNOTSUPP, rustix::io::Errno::XDEV] {
            check_sparse_copy(|source, destination| {
                FICLONE_ERROR.set(Some(error));
                let result = copy_contents(source, destination);
                assert!(FICLONE_ERROR.take().is_none(), "copy did not attempt FICLONE");
                result
            })?;
        }
        Ok(())
    }
    fn check_sparse_copy(copy: impl FnOnce(&File, &File) -> io::Result<()>) -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        source.write_all(b"first")?;
        source.seek(io::SeekFrom::Start(8 << 20))?;
        source.write_all(b"last")?;
        source.seek(io::SeekFrom::Start(3))?;
        let mut destination = fixture.file("destination")?;
        destination.write_all(&vec![0x7f; 16 << 20])?;
        destination.seek(io::SeekFrom::Start(17))?;
        fs::hard_link(fixture.0.join("destination"), fixture.0.join("alias"))?;
        let inode = destination.metadata()?.ino();
        copy(&source, &destination)?;
        assert_eq!(source.stream_position()?, 3);
        assert_eq!(destination.stream_position()?, 17);
        assert_eq!(destination.metadata()?.ino(), inode);
        assert_eq!(destination.metadata()?.len(), (8 << 20) + 4);
        assert!(destination.metadata()?.blocks() * 512 < 1 << 20);
        let mut bytes = Vec::new();
        File::open(fixture.0.join("alias"))?.read_to_end(&mut bytes)?;
        assert_eq!(&bytes[..5], b"first");
        assert!(bytes[5..8 << 20].iter().all(|byte| *byte == 0));
        assert_eq!(&bytes[8 << 20..], b"last");
        Ok(())
    }
    #[test]
    fn refuses_same_inode_and_preserves_bytes() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        source.write_all(b"keep")?;
        fs::hard_link(fixture.0.join("source"), fixture.0.join("alias"))?;
        let target = fs::OpenOptions::new().write(true).open(fixture.0.join("alias"))?;
        assert_eq!(copy_contents(&source, &target).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(fixture.0.join("source"))?, b"keep");
        Ok(())
    }
    #[test]
    fn failed_truncation_preserves_bytes_and_source_position() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        source.write_all(b"contents")?;
        source.seek(io::SeekFrom::Start(2))?;
        let mut destination = fixture.file("destination")?;
        destination.write_all(b"untouched")?;
        let readonly = File::open(fixture.0.join("destination"))?;
        assert!(copy_contents(&source, &readonly).is_err());
        assert_eq!(source.stream_position()?, 2);
        assert_eq!(fs::read(fixture.0.join("destination"))?, b"untouched");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn failed_source_seek_leaves_shrinking_destination_intact() -> io::Result<()> {
        use rustix::fs::{open, Mode, OFlags};

        let fixture = Fixture::new()?;
        fixture.file("source")?.write_all(b"short")?;
        let source = File::from(open(fixture.0.join("source"), OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?);
        fixture.file("destination")?.write_all(b"untouched")?;
        let destination = fs::OpenOptions::new().write(true).open(fixture.0.join("destination"))?;
        assert!(matches!(
            copy_contents(&source, &destination),
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::BADF.raw_os_error())
        ));
        assert_eq!(fs::read(fixture.0.join("destination"))?, b"untouched");
        Ok(())
    }
}
