//! Local store compatibility. This version is independent of encrypted remote formats.
//!
//! The volume files (`volumes.json`, `project-volumes.json` and each volume's `layr-volume.json`)
//! carry their own `format`, checked when they are read (`volumes.rs`). A store without them stays
//! at this format: no migration.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const FORMAT: u32 = 1;
pub const MARKER: &str = "store-version.json";
const LOCK: &str = ".layr-version.lock";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Version {
    pub format: u32,
    pub version: String,
    pub build: String,
}

impl Version {
    pub fn binary() -> Self {
        Self {
            format: FORMAT,
            version: env!("CARGO_PKG_VERSION").into(),
            build: option_env!("LAYR_BUILD_ID").unwrap_or("unknown").into(),
        }
    }

    pub(crate) fn description(&self) -> String {
        format!("format {} (layr {}, build {})", self.format, self.version, self.build)
    }

    pub fn require_compatible(&self, other: &Self, source: &str) -> Result<()> {
        if self.format != other.format {
            bail!("incompatible {source}: {}; this binary uses {}", other.description(), self.description());
        }
        Ok(())
    }
}

/// The shared lease prevents a store from changing format while a command or daemon holds it.
pub struct Guard {
    _file: File,
}

fn marker(root: &Path) -> Result<Option<Version>> {
    let path = root.join(MARKER);
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {} for {}", path.display(), Version::binary().description())),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > 4096 {
        bail!("invalid store version file {} for {}", path.display(), Version::binary().description());
    }
    Ok(Some(
        serde_json::from_reader(file)
            .with_context(|| format!("invalid store version file {} for {}", path.display(), Version::binary().description()))?,
    ))
}

fn lock_file(root: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(LOCK))
        .context("open the store version lock")?;
    if !file.metadata()?.is_file() {
        bail!("the store version lock is not a regular file");
    }
    Ok(file)
}

fn empty(root: &Path) -> Result<bool> {
    for entry in fs::read_dir(root)? {
        let name = entry?.file_name();
        let marker_temporary = name.to_str().is_some_and(|name| name.starts_with(".store-version.json.tmp-"));
        if name != LOCK && !marker_temporary {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Format 0 is a store without a marker. Its data has the layout of format 1: no change of the
/// layout came with the marker. Every store writes its machine identity at the first open, so
/// that file tells a layr store from another folder. Checked before any write.
fn has_layout(root: &Path) -> Result<bool> {
    match fs::symlink_metadata(root.join("machine")) {
        Ok(dir) if dir.is_dir() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    match fs::symlink_metadata(root.join("machine").join("id")) {
        Ok(file) => Ok(file.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// A folder that is not a layr store: not empty, no machine identity and no marker. The marker
/// is read again here: a concurrent first open writes it and then starts the machine folder, so a
/// caller that found no marker a moment ago can see that half-made store.
fn foreign(root: &Path) -> Result<bool> {
    Ok(!empty(root)? && !has_layout(root)? && marker(root)?.is_none())
}

fn unmarked_error() -> anyhow::Error {
    anyhow::anyhow!(
        "incompatible store: format 0 (no version marker, no machine/id); this binary uses {}",
        Version::binary().description()
    )
}

fn unwritable_error() -> anyhow::Error {
    anyhow::anyhow!(
        "incompatible store: format 0 (no version marker) is not writable, so its marker cannot be written; give write access to the store root and run the command again; this binary uses {}",
        Version::binary().description()
    )
}

/// Name the fix when the first open of an unmarked store cannot write its marker or lock file.
fn writable(error: anyhow::Error) -> anyhow::Error {
    let denied = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(libc::EROFS))
    });
    if denied {
        unwritable_error()
    } else {
        error
    }
}

impl Guard {
    pub fn open(root: &Path) -> Result<Self> {
        let binary = Version::binary();
        if let Some(stored) = marker(root)? {
            // Refuse before opening a lock file or changing machine/project data.
            binary.require_compatible(&stored, "store")?;
            let file = lock_file(root, true)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
            let stored = marker(root)?.context("the store version disappeared; refuse access")?;
            binary.require_compatible(&stored, "store")?;
            return Ok(Self { _file: file });
        }
        // No marker. A folder that is not a layr store is refused before the lock file is created.
        // A concurrent first open can write the marker and start the machine folder between the
        // check above and this one: read the marker again before refusing.
        if foreign(root)? {
            return Err(unmarked_error());
        }
        if marker(root)?.is_some() {
            return Self::open(root);
        }
        // Concurrent first opens queue on the exclusive lease. The first one writes the marker,
        // and the others find it. A concurrent initializer's temporary marker is not store data.
        let file = lock_file(root, true).map_err(writable)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
        match marker(root)? {
            Some(stored) => binary.require_compatible(&stored, "store")?,
            None => {
                if !empty(root)? && !has_layout(root)? {
                    return Err(unmarked_error());
                }
                crate::fsutil::atomic_write(&root.join(MARKER), &serde_json::to_vec(&binary)?, 0o644).map_err(writable)?;
            }
        }
        // The marker exists now, so every later open takes the shared lease of a marked store.
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
        let stored = marker(root)?.context("the store version disappeared; refuse access")?;
        binary.require_compatible(&stored, "store")?;
        Ok(Self { _file: file })
    }

    pub fn ready(&self, root: &Path) -> Result<()> {
        rustix::fs::flock(&self._file, rustix::fs::FlockOperation::LockShared)?;
        let stored = marker(root)?.context("the store version disappeared; refuse access")?;
        Version::binary().require_compatible(&stored, "store")
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Upgrade {
    pub metadata_only: bool,
    pub from_format: u32,
    pub to_format: u32,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub binary: Version,
    pub store: Option<Version>,
    pub compatible: bool,
    pub state: &'static str,
    pub upgrade: Option<Upgrade>,
}

/// Inspect without Store::open, locks, directory creation, or database migrations.
pub fn inspect(root: &Path) -> Result<Report> {
    let binary = Version::binary();
    let store = marker(root)?;
    let (state, compatible, upgrade) = match &store {
        Some(stored) if stored.format == binary.format => ("compatible", true, None),
        Some(_) => ("incompatible", false, None),
        None => match fs::metadata(root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ("absent", true, None),
            Err(e) => return Err(e.into()),
            Ok(_) if empty(root)? => ("empty", true, None),
            // The next open writes the marker (when the store is writable).
            Ok(_) if has_layout(root)? => {
                ("unmarked", true, Some(Upgrade { metadata_only: true, from_format: 0, to_format: FORMAT }))
            }
            Ok(_) => ("incompatible", false, None),
        },
    };
    Ok(Report { binary, store, compatible, state, upgrade })
}

pub fn run(root: &Path, args: &[String]) -> Result<i32> {
    let options = crate::args::parse(&crate::args::Spec::new().flag("--json"), args)?;
    if !options.pos.is_empty() || !options.paths.is_empty() {
        bail!("usage: layr store-version [--json]");
    }
    let report = inspect(root)?;
    if options.has("--json") {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("binary: {}", report.binary.description());
        match report.store {
            Some(stored) => println!("store: {} ({})", stored.description(), report.state),
            None => println!("store: {}", report.state),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod race_tests {
    use super::*;

    /// A store that a concurrent first open is making (marker written, machine folder without its
    /// id yet) is not a foreign folder; a folder with other files and no marker is.
    #[test]
    fn a_half_made_store_is_not_foreign() {
        let root = std::env::temp_dir().join(format!("layr-marker-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("machine")).unwrap();
        fs::write(root.join(MARKER), serde_json::to_vec(&Version::binary()).unwrap()).unwrap();
        assert!(!foreign(&root).unwrap(), "a marker and a machine folder without its id is a store being made");
        fs::remove_file(root.join(MARKER)).unwrap();
        fs::write(root.join("notes.txt"), b"x").unwrap();
        assert!(foreign(&root).unwrap(), "other files and no marker is a foreign folder");
        fs::remove_dir_all(&root).unwrap();
    }
}
