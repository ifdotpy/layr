//! Times codecs and filesystem timestamp operations.
pub use layr_object_format::times::*;

/// Nanoseconds since the epoch of a modification time.
pub fn ns_of(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime().wrapping_mul(1_000_000_000).wrapping_add(meta.mtime_nsec())
}

/// Set the modification time of `path` (not followed if it is a link); the access time stays.
pub fn set(path: &std::path::Path, ns: i64) -> std::io::Result<()> {
    let ts = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
        last_modification: rustix::fs::Timespec {
            tv_sec: ns.div_euclid(1_000_000_000) as _,
            tv_nsec: ns.rem_euclid(1_000_000_000) as _,
        },
    };
    Ok(rustix::fs::utimensat(rustix::fs::CWD, path, &ts, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?)
}
