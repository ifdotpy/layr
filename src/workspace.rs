//! Local folder bindings. A user-visible locator never grants store access.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    pub version: u32,
    pub store: PathBuf,
    pub project: String,
    pub project_id: String,
    pub line: String,
    pub line_id: String,
    pub binding_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub locator: Locator,
    pub folder: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub owner: u32,
}

fn regular_json<T: serde::de::DeserializeOwned>(file: fs::File) -> Result<T> {
    use std::io::Read;
    const LIMIT: u64 = 16 * 1024;
    let m = file.metadata()?;
    if !m.is_file() || m.len() > LIMIT {
        bail!("invalid workspace metadata file");
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        bail!("workspace metadata exceeds the size limit");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

thread_local! {
    static ROOTS: std::cell::RefCell<std::collections::BTreeMap<PathBuf, Binding>> = Default::default();
}

pub fn check_open_root(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ROOTS.with(|roots| match roots.borrow().get(path) {
        Some(binding) => binding.validate_root(metadata),
        None => Ok(()),
    })
}

pub fn is_input_root(path: &Path) -> bool {
    ROOTS.with(|roots| roots.borrow().contains_key(path))
}

pub fn protect_root(path: &Path) -> Result<()> {
    ROOTS.with(|roots| {
        if roots.borrow().keys().any(|root| root == path || root.starts_with(path)) {
            bail!("the input workspace folder cannot be removed or moved by layr");
        }
        Ok(())
    })
}

pub fn load(meta: &Path, store: &Path, project: &str, id: &str) -> Result<Option<Binding>> {
    let path = meta.join("workspace.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let binding: Binding = regular_json(crate::tree::Tree::open(meta)?.open_read("workspace.json")?)?;
    binding.validate(store, project, id)?;
    ROOTS.with(|roots| roots.borrow_mut().insert(binding.folder.clone(), binding.clone()));
    Ok(Some(binding))
}

/// The project lock serializes this transaction. The pending record lets a retry
/// reuse the same identity after interruption between the two metadata writes.
pub fn attach(meta: &Path, requested: Binding) -> Result<Binding> {
    let l = &requested.locator;
    if let Some(old) = load(meta, &l.store, &l.project, &l.project_id)? {
        if old.folder != requested.folder
            || old.locator.line != l.line
            || old.locator.line_id != l.line_id
            || old.owner != requested.owner
        {
            bail!("project already has a different input workspace");
        }
        old.validate_root(&fs::symlink_metadata(&requested.folder)?)?;
        return Ok(old);
    }
    let pending = meta.join("workspace.pending.json");
    let binding = if pending.try_exists()? {
        let old: Binding = regular_json(crate::tree::Tree::open(meta)?.open_read("workspace.pending.json")?)?;
        if old.folder != requested.folder
            || old.locator.store != l.store
            || old.locator.project_id != l.project_id
            || old.locator.project != l.project
            || old.locator.line != l.line
            || old.locator.line_id != l.line_id
            || old.owner != requested.owner
        {
            bail!("a different workspace attachment is pending");
        }
        old.validate_root(&fs::symlink_metadata(&requested.folder)?)?;
        old
    } else {
        if requested.folder.join(".layr").symlink_metadata().is_ok() {
            bail!("input folder already contains .layr");
        }
        crate::fsutil::atomic_write(&pending, &serde_json::to_vec(&requested)?, 0o600)?;
        requested
    };
    let user = crate::privs::user(binding.owner)?;
    crate::privs::as_user_fs(user.uid, user.gid, || {
        let control = binding.folder.join(".layr");
        match fs::create_dir(&control) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&control)?.is_dir() {
                    bail!("workspace control directory was replaced");
                }
            }
            Err(e) => return Err(e.into()),
        }
        binding.validate_root(&crate::tree::Tree::open(&binding.folder)?.root_metadata()?)?;
        let marker = control.join("link.json");
        if marker.symlink_metadata().is_ok() {
            binding.validate(&binding.locator.store, &binding.locator.project, &binding.locator.project_id)?;
        } else {
            crate::fsutil::atomic_write(&marker, &serde_json::to_vec(&binding.locator)?, 0o600)?;
        }
        Ok(())
    })?;
    binding.validate(&binding.locator.store, &binding.locator.project, &binding.locator.project_id)?;
    crate::fsutil::atomic_write(&meta.join("workspace.json"), &serde_json::to_vec(&binding)?, 0o600)?;
    fs::remove_file(pending)?;
    fs::File::open(meta)?.sync_all()?;
    Ok(binding)
}

impl Locator {
    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!("unsupported workspace link version");
        }
        crate::store::check_name("project", &self.project)?;
        crate::store::check_name("line", &self.line)?;
        uuid::Uuid::parse_str(&self.project_id).context("invalid linked project identity")?;
        uuid::Uuid::parse_str(&self.line_id).context("invalid linked line identity")?;
        uuid::Uuid::parse_str(&self.binding_id).context("invalid workspace binding identity")?;
        if !self.store.is_absolute() || self.store.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("workspace store must be an absolute path without parent components");
        }
        Ok(())
    }
}

impl Binding {
    pub fn new(
        store: &Path,
        project: &str,
        project_id: &str,
        line: &str,
        line_id: &str,
        folder: &Path,
        owner: u32,
    ) -> Result<Self> {
        let folder = folder.canonicalize()?;
        let store = store.canonicalize()?;
        if folder.starts_with(&store) || store.starts_with(&folder) {
            bail!("workspace and store must be separate directory trees");
        }
        let m = fs::symlink_metadata(&folder)?;
        if !m.is_dir() || m.uid() != owner {
            bail!("workspace folder must belong to its line owner");
        }
        let locator = Locator {
            version: VERSION,
            store,
            project: project.into(),
            project_id: project_id.into(),
            line: line.into(),
            line_id: line_id.into(),
            binding_id: uuid::Uuid::now_v7().to_string(),
        };
        locator.validate()?;
        Ok(Self { locator, folder, device: m.dev(), inode: m.ino(), owner })
    }

    /// Match the opened root, not just a path that its owner can replace.
    pub fn validate_root(&self, m: &fs::Metadata) -> Result<()> {
        if !m.is_dir() || m.dev() != self.device || m.ino() != self.inode || m.uid() != self.owner {
            bail!("linked workspace folder changed; repair its binding before use");
        }
        Ok(())
    }

    pub fn validate(&self, store: &Path, project: &str, project_id: &str) -> Result<()> {
        self.locator.validate()?;
        if self.locator.store != store.canonicalize()? || self.locator.project != project || self.locator.project_id != project_id
        {
            bail!("workspace binding belongs to another store or project");
        }
        if self.folder.starts_with(&self.locator.store)
            || self.locator.store.starts_with(&self.folder)
            || self.folder.canonicalize()? != self.folder
        {
            bail!("workspace binding path is no longer separate and canonical");
        }
        let tree = crate::tree::Tree::open(&self.folder)?;
        self.validate_root(&tree.root_metadata()?)?;
        let found: Locator = regular_json(tree.open_read(".layr/link.json")?)?;
        if found != self.locator {
            bail!("workspace locator does not match the store binding");
        }
        Ok(())
    }
}

/// Discover a locator only. The caller must validate the authoritative store binding.
pub fn discover(cwd: &Path) -> Result<Option<(PathBuf, Locator)>> {
    for root in cwd.ancestors() {
        let dir = root.join(".layr");
        match fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
            Ok(m) if !m.is_dir() => bail!("workspace link directory is not a directory"),
            Ok(_) => {}
        }
        let locator: Locator = regular_json(crate::tree::Tree::open(root)?.open_read(".layr/link.json")?)?;
        locator.validate()?;
        return Ok(Some((root.to_path_buf(), locator)));
    }
    Ok(None)
}

/// Capture an ordinary folder through a disposable managed tree. The copy runs
/// with the folder owner's permissions, including when the service is root.
/// The caller holds the project lock and supplies private managed paths.
pub fn capture(binding: &Binding, scratch: &Path, dst: &Path, readonly: bool) -> Result<()> {
    capture_part(binding, &binding.folder, scratch, dst, readonly, &[])
}

pub fn capture_part(
    binding: &Binding,
    source: &Path,
    scratch: &Path,
    dst: &Path,
    readonly: bool,
    excluded: &[String],
) -> Result<()> {
    use std::process::{Command, Stdio};
    binding.validate(&binding.locator.store, &binding.locator.project, &binding.locator.project_id)?;
    let user = crate::privs::user(binding.owner)?;
    if !source.starts_with(&binding.folder) {
        bail!("capture path is outside the input workspace");
    }
    let before = crate::privs::as_user_fs(user.uid, user.gid, || {
        let relative = source.strip_prefix(&binding.folder)?.to_str().context("non-UTF-8 capture path")?;
        crate::tree::Tree::open(&binding.folder)?.list(relative)?;
        fingerprint(source, excluded)
    })?;
    crate::storage::create_subvolume(scratch)?;
    let mut list_live = false;
    let result = (|| -> Result<()> {
        crate::fsutil::chown_nofollow(scratch, user.uid, user.gid)?;
        let entries: Vec<_> = before.keys().filter(|p| !p.as_os_str().is_empty()).map(|p| PathBuf::from(".").join(p)).collect();
        if !entries.is_empty() {
            let mut pack = Command::new("/usr/bin/tar");
            pack.args(["-c", "-f", "-"]);
            #[cfg(target_os = "linux")]
            pack.args(["--sparse", "--xattrs", "--acls"]);
            // A literal NUL-separated list avoids shell/glob expansion and the
            // operating system limit on command argument size.
            use std::io::Write;
            use std::os::unix::ffi::OsStrExt;
            let list_path = scratch.with_extension("list");
            use std::os::unix::fs::OpenOptionsExt;
            let mut list = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&list_path)?;
            list_live = true;
            for entry in &entries {
                list.write_all(entry.as_os_str().as_bytes())?;
                list.write_all(&[0])?;
            }
            drop(list);
            crate::fsutil::chown_nofollow(&list_path, user.uid, user.gid)?;
            pack.arg("-C").arg(source).args(["--null", "--no-recursion", "-T"]).arg(&list_path);
            pack.stdout(Stdio::piped());
            crate::privs::command_as(&mut pack, user.uid, &[])?;
            let mut sender = pack.spawn().context("read the input folder")?;
            let mut unpack = Command::new("/usr/bin/tar");
            unpack.args(["-x", "-p", "-f", "-"]);
            #[cfg(target_os = "linux")]
            unpack.args(["--xattrs", "--acls"]);
            unpack.arg("-C").arg(scratch);
            unpack.stdin(Stdio::from(sender.stdout.take().context("capture pipe")?));
            let receive = crate::privs::command_as(&mut unpack, user.uid, &[]).and_then(|_| Ok(unpack.status()?));
            if receive.is_err() {
                let _ = sender.kill();
            }
            let sent = sender.wait()?;
            fs::remove_file(list_path)?;
            list_live = false;
            if !receive?.success() || !sent.success() {
                bail!("cannot capture input folder: tar failed");
            }
        }
        let after = crate::privs::as_user_fs(user.uid, user.gid, || fingerprint(source, excluded))?;
        binding.validate(&binding.locator.store, &binding.locator.project, &binding.locator.project_id)?;
        if before != after {
            bail!("input folder changed during capture; retry when writes stop");
        }
        crate::storage::snapshot(scratch, dst, readonly)
    })();
    if list_live {
        let _ = fs::remove_file(scratch.with_extension("list"));
    }
    let cleanup = crate::storage::delete_tree(scratch);
    result?;
    cleanup
}

// Reads must not hide permission failures or changes below a directory. Atime
// changes during a read and is deliberately absent from this comparison.
fn fingerprint(root: &Path, excluded: &[String]) -> Result<std::collections::BTreeMap<PathBuf, Vec<i128>>> {
    fn visit(
        root: &Path,
        rel: &Path,
        out: &mut std::collections::BTreeMap<PathBuf, Vec<i128>>,
        excluded: &[String],
    ) -> Result<()> {
        if excluded.iter().any(|p| rel == Path::new(p) || rel.starts_with(Path::new(p))) {
            return Ok(());
        }
        let path = root.join(rel);
        let m = fs::symlink_metadata(&path)?;
        if !m.is_dir() && !m.is_file() && !m.file_type().is_symlink() {
            bail!("cannot capture special file {}", path.display());
        }
        out.insert(
            rel.into(),
            vec![
                m.dev() as i128,
                m.ino() as i128,
                m.mode() as i128,
                m.uid() as i128,
                m.gid() as i128,
                m.nlink() as i128,
                m.len() as i128,
                m.mtime() as i128,
                m.mtime_nsec() as i128,
                m.ctime() as i128,
                m.ctime_nsec() as i128,
            ],
        );
        if m.is_dir() {
            for e in fs::read_dir(path)? {
                let e = e?;
                if rel.as_os_str().is_empty() && (e.file_name() == ".layr" || e.file_name() == ".git") {
                    continue;
                }
                visit(root, &rel.join(e.file_name()), out, excluded)?;
            }
        }
        Ok(())
    }
    let mut out = std::collections::BTreeMap::new();
    visit(root, Path::new(""), &mut out, excluded)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a backend test filesystem"]
    fn capture_keeps_source_and_file_structure() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let volume = std::env::var_os(if cfg!(target_os = "macos") { "LAYR_APFS_TEST_ROOT" } else { "LAYR_OVERLAYFS_TEST_ROOT" })
            .context("backend test root")?;
        let root = PathBuf::from(volume).join(format!("layr-capture-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let store = root.join("store");
            let folder = root.join("source");
            fs::create_dir(&store)?;
            #[cfg(feature = "overlayfs")]
            crate::storage::recover(&store)?;
            fs::create_dir(&folder)?;
            fs::create_dir(folder.join(".layr"))?;
            fs::create_dir(folder.join(".git"))?;
            fs::write(folder.join(".git/config"), b"private git metadata")?;
            fs::write(folder.join("a"), b"original")?;
            fs::hard_link(folder.join("a"), folder.join("b"))?;
            fs::set_permissions(folder.join("a"), fs::Permissions::from_mode(0o444))?;
            std::os::unix::fs::symlink("a", folder.join("link"))?;
            let sparse = fs::File::create(folder.join("sparse"))?;
            sparse.set_len(16 * 1024 * 1024)?;
            drop(sparse);
            let id = uuid::Uuid::now_v7().to_string();
            let binding = Binding::new(&store, "app", &id, "main", &id, &folder, fs::metadata(&folder)?.uid())?;
            fs::write(folder.join(".layr/link.json"), serde_json::to_vec(&binding.locator)?)?;
            let dst = store.join("state");
            capture(&binding, &store.join("scratch"), &dst, false)?;
            assert!(!store.join("scratch").exists());
            assert!(!dst.join(".layr").exists());
            assert!(!dst.join(".git").exists());
            assert_eq!(fs::read(dst.join("a"))?, b"original");
            assert_eq!(fs::metadata(dst.join("a"))?.ino(), fs::metadata(dst.join("b"))?.ino());
            assert_eq!(fs::metadata(dst.join("a"))?.mode() & 0o777, 0o444);
            assert_eq!(fs::read_link(dst.join("link"))?, PathBuf::from("a"));
            assert_eq!(fs::metadata(dst.join("sparse"))?.len(), 16 * 1024 * 1024);
            assert!(fs::metadata(dst.join("sparse"))?.blocks() * 512 < 1024 * 1024);
            fs::set_permissions(dst.join("a"), fs::Permissions::from_mode(0o644))?;
            fs::write(dst.join("a"), b"changed")?;
            assert_eq!(fs::read(folder.join("a"))?, b"original");
            binding.validate(&store, "app", &id)?;
            crate::storage::delete_tree(&dst)?;
            Ok(())
        })();
        // This fixture belongs to this test. Backend deletion removes its mounts.
        if root.join("store/state").exists() {
            crate::storage::delete_tree(&root.join("store/state"))?;
        }
        if root.join("store/scratch").exists() {
            crate::storage::delete_tree(&root.join("store/scratch"))?;
        }
        fs::remove_dir_all(&root)?;
        result
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn input_names_do_not_overwrite_case_aliases() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-case-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let store = root.join("store");
            let source = root.join("source");
            fs::create_dir(&store)?;
            fs::create_dir(&source)?;
            fs::write(source.join("Mixed"), b"original")?;
            fs::create_dir(source.join("Folder"))?;
            let id = uuid::Uuid::now_v7().to_string();
            let owner = fs::metadata(&source)?.uid();
            let binding = Binding::new(&store, "app", &id, "main", &id, &source, owner)?;
            attach(&store, binding.clone())?;
            load(&store, &store, "app", &id)?;
            let tree = crate::tree::Tree::open(&binding.folder)?;
            if source.join("mixed").exists() {
                assert!(tree.check_names(["first", "Mixed", "mixed"]).is_err());
                assert!(tree.check_names(["new/Name", "new/name"]).is_err());
                assert!(tree.check_names(["Folder/one", "folder/two"]).is_err());
                assert!(!source.join("first").exists());
                assert!(tree.create("mixed", 0o600).is_err());
                assert!(tree.create("folder/new", 0o600).is_err());
                assert_eq!(fs::read(source.join("Mixed"))?, b"original");
                assert!(!source.join("Folder/new").exists());
            } else {
                // A case-sensitive host can represent the second name safely.
                tree.create("mixed", 0o600)?;
                assert_eq!(fs::read(source.join("Mixed"))?, b"original");
            }
            Ok(())
        })();
        fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn attachment_resumes_with_the_same_identity() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-attach-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let store = root.join("store");
            let source = root.join("source");
            let other = root.join("other");
            for p in [&store, &source, &other] {
                fs::create_dir(p)?;
            }
            let id = uuid::Uuid::now_v7().to_string();
            let owner = fs::metadata(&source)?.uid();
            let pending = Binding::new(&store, "app", &id, "main", &id, &source, owner)?;
            // Crash after the store write but before the source locator exists.
            fs::write(store.join("workspace.pending.json"), serde_json::to_vec(&pending)?)?;
            let retry = Binding::new(&store, "app", &id, "main", &id, &source, owner)?;
            let done = attach(&store, retry)?;
            assert_eq!(done.locator, pending.locator);
            assert!(!store.join("workspace.pending.json").exists());
            let again = Binding::new(&store, "app", &id, "main", &id, &source, owner)?;
            assert_eq!(attach(&store, again)?.locator, done.locator);
            let wrong = Binding::new(&store, "app", &id, "main", &id, &other, owner)?;
            assert!(attach(&store, wrong).is_err());
            assert!(!other.join(".layr").exists());
            Ok(())
        })();
        fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn locator_is_not_an_authority() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-link-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let store = root.join("store");
            let folder = root.join("source");
            fs::create_dir(&store)?;
            fs::create_dir(&folder)?;
            fs::create_dir(folder.join(".layr"))?;
            fs::create_dir(folder.join("src"))?;
            let id = uuid::Uuid::now_v7().to_string();
            let owner = fs::metadata(&folder)?.uid();
            let binding = Binding::new(&store, "app", &id, "main", &id, &folder, owner)?;
            let marker = folder.join(".layr/link.json");
            fs::write(&marker, serde_json::to_vec(&binding.locator)?)?;
            binding.validate(&store, "app", &id)?;
            assert_eq!(discover(&folder.join("src"))?.unwrap().1, binding.locator);
            assert!(binding.validate(&store, "other", &id).is_err());
            let mut forged = binding.locator.clone();
            forged.binding_id = uuid::Uuid::now_v7().to_string();
            fs::write(&marker, serde_json::to_vec(&forged)?)?;
            assert!(binding.validate(&store, "app", &id).is_err());
            fs::write(&marker, serde_json::to_vec(&binding.locator)?)?;
            fs::rename(folder.join(".layr"), folder.join("control"))?;
            std::os::unix::fs::symlink("control", folder.join(".layr"))?;
            assert!(binding.validate(&store, "app", &id).is_err());
            assert!(discover(&folder).is_err());
            fs::remove_file(folder.join(".layr"))?;
            fs::rename(folder.join("control"), folder.join(".layr"))?;
            fs::rename(&marker, folder.join("marker"))?;
            std::os::unix::fs::symlink("../marker", &marker)?;
            assert!(binding.validate(&store, "app", &id).is_err());
            assert!(discover(&folder).is_err());
            fs::remove_file(&marker)?;
            fs::rename(folder.join("marker"), &marker)?;
            fs::rename(&folder, root.join("moved"))?;
            fs::create_dir(&folder)?;
            assert!(binding.validate(&store, "app", &id).is_err());
            fs::remove_dir(&folder)?;
            std::os::unix::fs::symlink(root.join("moved"), &folder)?;
            assert!(binding.validate(&store, "app", &id).is_err());
            Ok(())
        })();
        fs::remove_dir_all(&root)?;
        result
    }
}
