use rustix::fs::{seek, SeekFrom};
use std::{
    fs::File,
    io,
    os::unix::fs::{FileExt, MetadataExt},
};

/// Replace file content, preserving the destination inode and both descriptor positions.
/// Native cloning is attempted before mutation. The executor owns its sparse/dense fallback;
/// an error can follow partial writes and must never be treated as an unavailable clone.
pub fn copy_contents(source: &File, destination: &File) -> io::Result<()> {
    let from = source.metadata()?;
    let to = destination.metadata()?;
    if !from.is_file() || !to.is_file() || (from.dev(), from.ino()) == (to.dev(), to.ino()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "content copy requires distinct regular files"));
    }
    #[cfg(target_os = "linux")]
    if matches!(clone_contents(source, destination)?, layr_fs_core::Attempt::Applied(())) {
        return Ok(());
    }
    copy_sparse(source, destination, from.len())
}

#[cfg(target_os = "linux")]
pub fn clone_contents(source: &File, destination: &File) -> io::Result<layr_fs_core::Attempt<()>> {
    match rustix::fs::ioctl_ficlone(destination, source) {
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
fn copy_sparse(source: &File, destination: &File, size: u64) -> io::Result<()> {
    let position = Position { file: source, offset: seek(source, SeekFrom::Current(0))? };
    destination.set_len(0)?;
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
    fn sparse_fallback_truncates_preserves_inode_aliases_and_positions() -> io::Result<()> {
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
        // Exercise the sparse executor even on hosts where FICLONE always succeeds.
        copy_sparse(&source, &destination, source.metadata()?.len())?;
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
    fn failed_sparse_copy_restores_source_position() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut source = fixture.file("source")?;
        source.write_all(b"contents")?;
        source.seek(io::SeekFrom::Start(2))?;
        let mut destination = fixture.file("destination")?;
        destination.write_all(b"untouched")?;
        let readonly = File::open(fixture.0.join("destination"))?;
        assert!(copy_sparse(&source, &readonly, 8).is_err());
        assert_eq!(source.stream_position()?, 2);
        assert_eq!(fs::read(fixture.0.join("destination"))?, b"untouched");
        Ok(())
    }
}
