//! Local store compatibility. This version is independent of encrypted remote formats.
//!
//! The volume files (`volumes.json`, `project-volumes.json` and each volume's `layr-volume.json`)
//! carry their own `format`, checked when they are read (`volumes.rs`). A store without them stays
//! at this format: no migration.
use anyhow::{bail, Context, Result};
pub use layr_api::Version;
use layr_api::{Peer, SchemaVersion, StoreState, StoreVersionReport as Report, Upgrade, API_MAJOR};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::num::NonZeroU32;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Format 2 (owner decision 2026-11-15, docs/overlays.md section 1): the store can hold the tree of
/// patch overlays. A binary of format 1 would save a private edit into `main`, so it must refuse the
/// store. The data of format 1 is the same; the first open of this binary writes the new marker.
pub const FORMAT: u32 = 2;
/// The older format that this binary upgrades in place (metadata only).
const UPGRADES_FROM: u32 = 1;
pub const MARKER: &str = "store-version.json";
/// The largest marker file that is read.
const MARKER_MAX: usize = 4096;
const LOCK: &str = ".layr-version.lock";

pub fn binary() -> Version {
    // The version of this binary (the layr package), not of the layr-api crate, which `release prepare` does not bump.
    Version {
        format: FORMAT,
        version: env!("CARGO_PKG_VERSION").into(),
        build: option_env!("LAYR_BUILD_ID").unwrap_or("unknown").into(),
    }
}

/// Advertise the binary's database schema without changing the store marker contract.
pub fn peer() -> Result<Peer> {
    let schema = NonZeroU32::new(u32::try_from(crate::db::SCHEMA_VERSION)?).context("database schema must be positive")?;
    Ok(Peer { version: binary(), schema: SchemaVersion::new(schema) })
}

/// Peers must agree on both the store format and the derived database schema.
pub fn require_peer_compatible(binary: &Peer, other: &Peer, source: &str) -> Result<()> {
    if binary.version.format != other.version.format || binary.schema != other.schema {
        bail!(
            "incompatible {source}: {}, database schema {}; this binary uses {}, database schema {}; upgrade both the CLI and the daemon",
            description(&other.version),
            other.schema.get(),
            description(&binary.version),
            binary.schema.get()
        );
    }
    Ok(())
}

pub(crate) fn description(version: &Version) -> String {
    format!("format {} (layr {}, build {})", version.format, version.version, version.build)
}

pub fn require_compatible(binary: &Version, other: &Version, source: &str) -> Result<()> {
    if binary.format != other.format {
        bail!("incompatible {source}: {}; this binary uses {}", description(other), description(binary));
    }
    Ok(())
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
        Err(e) => return Err(e).with_context(|| format!("read {} for {}", path.display(), description(&binary()))),
    };
    let invalid = || format!("invalid store version file {} for {}", path.display(), description(&binary()));
    if !file.metadata()?.is_file() {
        bail!("{}", invalid());
    }
    // One read of at most 4097 bytes: a marker that grew past 4096 bytes after the check is refused, not cut.
    let mut bytes = Vec::with_capacity(MARKER_MAX + 1);
    file.take(MARKER_MAX as u64 + 1).read_to_end(&mut bytes).with_context(invalid)?;
    if bytes.len() > MARKER_MAX {
        bail!("{}", invalid());
    }
    Ok(Some(serde_json::from_slice(&bytes).with_context(invalid)?))
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

/// True when `root` holds a layr store of any format: the version marker, or the machine identity of
/// a store from before the marker (format 0). Reads only; an invalid marker file is an error.
pub fn is_store(root: &Path) -> Result<bool> {
    Ok(marker(root)?.is_some() || has_layout(root)?)
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
        description(&binary())
    )
}

fn unwritable_error() -> anyhow::Error {
    anyhow::anyhow!(
        "incompatible store: format 0 (no version marker) is not writable, so its marker cannot be written; give write access to the store root and run the command again; this binary uses {}",
        description(&binary())
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
        let binary = binary();
        if let Some(stored) = marker(root)? {
            if stored.format == UPGRADES_FROM {
                return Self::upgrade(root);
            }
            // Refuse before opening a lock file or changing machine/project data.
            require_compatible(&binary, &stored, "store")?;
            let file = lock_file(root, true)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
            let stored = marker(root)?.context("the store version disappeared; refuse access")?;
            require_compatible(&binary, &stored, "store")?;
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
            Some(stored) => require_compatible(&binary, &stored, "store")?,
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
        require_compatible(&binary, &stored, "store")?;
        Ok(Self { _file: file })
    }

    /// A marker of format 1: write the marker of this format under the exclusive lease, then take the
    /// shared one. A concurrent upgrade finds the new marker. Only the marker changes.
    fn upgrade(root: &Path) -> Result<Self> {
        let binary = binary();
        let file = lock_file(root, true).map_err(writable)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
        let stored = marker(root)?.context("the store version disappeared; refuse access")?;
        if stored.format == UPGRADES_FROM {
            crate::fsutil::atomic_write(&root.join(MARKER), &serde_json::to_vec(&binary)?, 0o644).map_err(writable)?;
        }
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
        let stored = marker(root)?.context("the store version disappeared; refuse access")?;
        require_compatible(&binary, &stored, "store")?;
        Ok(Self { _file: file })
    }
}

/// Inspect without Store::open, locks, directory creation, or database migrations.
pub fn inspect(root: &Path) -> Result<Report> {
    let binary = binary();
    let store = marker(root)?;
    let (state, upgrade) = match &store {
        Some(stored) if stored.format == binary.format => (StoreState::Compatible, None),
        Some(stored) if stored.format == UPGRADES_FROM => {
            (StoreState::Upgradable, Some(Upgrade { metadata_only: true, from_format: UPGRADES_FROM, to_format: FORMAT }))
        }
        Some(_) => (StoreState::Incompatible, None),
        None => match fs::metadata(root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StoreState::Absent, None),
            Err(e) => return Err(e.into()),
            Ok(_) if empty(root)? => (StoreState::Empty, None),
            // The next open writes the marker (when the store is writable).
            Ok(_) if has_layout(root)? => {
                (StoreState::Unmarked, Some(Upgrade { metadata_only: true, from_format: 0, to_format: FORMAT }))
            }
            Ok(_) => (StoreState::Incompatible, None),
        },
    };
    Ok(Report::new(binary, store, state, upgrade, API_MAJOR)?)
}

pub fn run(root: &Path, args: &[String]) -> Result<i32> {
    let options = crate::args::parse(&crate::args::Spec::new().flag("--json"), args)?;
    if !options.pos.is_empty() || !options.paths.is_empty() {
        bail!("usage: layr store-version [--json]");
    }
    let report = inspect(root)?;
    // The daemon of this user, when one answers: its binary can differ from this one (an install
    // without a daemon restart) while the format matches.
    let daemon = match crate::service::hello(&crate::service::user_daemon_socket()) {
        crate::service::Hello::Absent => None,
        crate::service::Hello::Busy => Some(serde_json::json!({"busy": true})),
        crate::service::Hello::Running { version: None, .. } => Some(serde_json::json!({"version": null})),
        crate::service::Hello::Running { version: Some(v), .. } => Some(serde_json::to_value(v)?),
    };
    if options.has("--json") {
        let typed = daemon.clone().and_then(|d| serde_json::from_value::<layr_api::Daemon>(d).ok());
        println!("{}", serde_json::to_string(&report.clone().with_daemon(typed))?);
    } else {
        let state = serde_json::to_value(report.state())?;
        let state = state.as_str().context("store state must serialize as a string")?;
        println!("binary: {}", description(report.binary()));
        match report.store() {
            Some(stored) => println!("store: {} ({})", description(stored), state),
            None => println!("store: {}", state),
        }
        match daemon.as_ref().map(|d| serde_json::from_value::<Version>(d.clone())) {
            None => {}
            Some(Ok(v)) => println!("daemon: {}", description(&v)),
            Some(Err(_)) if daemon.as_ref().is_some_and(|d| d["busy"] == true) => println!("daemon: busy"),
            Some(Err(_)) => println!("daemon: an older build that does not report its version"),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod race_tests {
    use super::*;

    #[test]
    fn report_advertises_api_major() {
        let root = std::env::temp_dir().join(format!("layr-api-absent-{}", std::process::id()));
        assert!(!root.exists());
        let report = serde_json::to_value(inspect(&root).unwrap()).unwrap();
        assert_eq!(report.get("api").and_then(serde_json::Value::as_u64), Some(u64::from(API_MAJOR)));
    }

    /// A store of format 1 is upgradable: the first open writes format 2, after which a binary of
    /// format 1 (the same check with its format) refuses it. A later format is refused unchanged.
    #[test]
    fn format_one_is_upgraded_and_then_refused_by_format_one() {
        let root = std::env::temp_dir().join(format!("layr-format-up-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("machine")).unwrap();
        let old = Version { format: 1, version: "0.47.0".into(), build: "old".into() };
        fs::write(root.join(MARKER), serde_json::to_vec(&old).unwrap()).unwrap();
        let report = serde_json::to_value(inspect(&root).unwrap()).unwrap();
        assert_eq!(report["state"], "upgradable");
        assert_eq!(report["upgrade"]["fromFormat"], 1);
        drop(Guard::open(&root).unwrap());
        let now = marker(&root).unwrap().unwrap();
        assert_eq!(now.format, FORMAT);
        assert!(require_compatible(&old, &now, "store").is_err(), "a binary of format 1 refuses the upgraded store");
        let later = Version { format: FORMAT + 1, version: "9".into(), build: "x".into() };
        fs::write(root.join(MARKER), serde_json::to_vec(&later).unwrap()).unwrap();
        assert!(Guard::open(&root).is_err());
        assert_eq!(marker(&root).unwrap().unwrap().format, FORMAT + 1, "a later format is never rewritten");
        fs::remove_dir_all(&root).unwrap();
    }

    /// The marker is read whole up to its size limit: 4096 bytes parse, one more byte is refused, not cut.
    #[test]
    fn marker_is_read_whole_up_to_its_limit() {
        let root = std::env::temp_dir().join(format!("layr-marker-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut json = serde_json::to_vec(&binary()).unwrap();
        json.resize(MARKER_MAX, b' ');
        fs::write(root.join(MARKER), &json).unwrap();
        assert_eq!(marker(&root).unwrap(), Some(binary()));
        json.push(b' ');
        fs::write(root.join(MARKER), &json).unwrap();
        assert!(format!("{:#}", marker(&root).unwrap_err()).contains("invalid store version file"));
        fs::remove_dir_all(&root).unwrap();
    }

    /// A store that a concurrent first open is making (marker written, machine folder without its
    /// id yet) is not a foreign folder; a folder with other files and no marker is.
    #[test]
    fn a_half_made_store_is_not_foreign() {
        let root = std::env::temp_dir().join(format!("layr-marker-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("machine")).unwrap();
        fs::write(root.join(MARKER), serde_json::to_vec(&binary()).unwrap()).unwrap();
        assert!(!foreign(&root).unwrap(), "a marker and a machine folder without its id is a store being made");
        fs::remove_file(root.join(MARKER)).unwrap();
        fs::write(root.join("notes.txt"), b"x").unwrap();
        assert!(foreign(&root).unwrap(), "other files and no marker is a foreign folder");
        fs::remove_dir_all(&root).unwrap();
    }
}
