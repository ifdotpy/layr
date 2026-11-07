//! Change notification for an indexed tree, with a barrier: `sync` returns after every change
//! made before the call has reached the sink. A query calls `sync` first, so an agent that writes
//! a file and then searches sees the file.
//!
//! macOS: an FSEvents stream with file events on its own dispatch queue; the barrier is
//! `FSEventStreamFlushSync`. Linux: inotify with one watch per folder; events are queued by the
//! kernel when the change happens, so the barrier drains the queue.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Receives root-relative changed paths. `rescan` asks for a rebuild of the whole tree (events
/// were lost).
pub trait Sink: Send + Sync + 'static {
    fn changed(&self, paths: Vec<PathBuf>, rescan: bool);
}

pub struct Watcher {
    imp: imp::Imp,
}

impl Watcher {
    /// Start watching `root` (a canonical path).
    pub fn start(root: &Path, sink: Arc<dyn Sink>) -> Result<Watcher> {
        Ok(Watcher { imp: imp::Imp::start(root, sink)? })
    }

    /// Deliver every change made before this call.
    pub fn sync(&self) {
        self.imp.sync();
    }
}

fn relative(root: &Path, abs: &Path) -> Option<PathBuf> {
    abs.strip_prefix(root).ok().map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{relative, Sink};
    use crate::sys::fsevents::{self as fse, Stream};
    use anyhow::{Context, Result};
    use std::path::Path;
    use std::sync::Arc;

    pub struct Imp {
        stream: Stream,
    }

    impl Imp {
        pub fn start(root: &Path, sink: Arc<dyn Sink>) -> Result<Imp> {
            let root2 = root.to_path_buf();
            let handler: fse::Handler = Box::new(move |batch| {
                let mut changed = Vec::new();
                let mut rescan = false;
                // A content or metadata change of a known file does not change the index: only
                // creations, removals, renames, folders, links and lost detail need a refresh
                // (and ignore files, whose content is rules).
                let namespace = fse::ITEM_CREATED
                    | fse::ITEM_REMOVED
                    | fse::ITEM_RENAMED
                    | fse::ITEM_IS_DIR
                    | fse::ITEM_IS_SYMLINK
                    | fse::MUST_SCAN_SUBDIRS;
                for (path, flag) in batch {
                    if flag & (fse::USER_DROPPED | fse::KERNEL_DROPPED | fse::ROOT_CHANGED) != 0 {
                        rescan = true;
                        continue;
                    }
                    let Some(rel) = relative(&root2, &path) else { continue };
                    let name = rel.file_name().map(|n| n.to_string_lossy().into_owned());
                    let rules = matches!(name.as_deref(), Some(".gitignore" | ".ignore" | ".rgignore"))
                        || rel == Path::new(".git/info/exclude");
                    if flag & namespace != 0 || rules {
                        changed.push(rel);
                    }
                }
                if !changed.is_empty() || rescan {
                    sink.changed(changed, rescan);
                }
            });
            let stream = Stream::start(root, handler).with_context(|| format!("cannot watch {}", root.display()))?;
            Ok(Imp { stream })
        }

        pub fn sync(&self) {
            self.stream.flush();
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{relative, Sink};
    use anyhow::{Context, Result};
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    use std::collections::HashMap;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, Weak};

    struct State {
        fd: OwnedFd,
        /// Watch descriptor to the root-relative folder.
        dirs: HashMap<i32, PathBuf>,
    }

    pub struct Imp {
        root: PathBuf,
        sink: Arc<dyn Sink>,
        state: Arc<Mutex<State>>,
    }

    fn flags() -> WatchFlags {
        WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::ONLYDIR
            | WatchFlags::DONT_FOLLOW
            | WatchFlags::EXCL_UNLINK
    }

    /// Watch `dir` and the folders below it that rg enters. Ignored and hidden folders
    /// (`target/`, `node_modules`, `.git`) are not watched: that keeps a JavaScript project
    /// below `fs.inotify.max_user_watches`, and `find` checks those folders by their times.
    /// The walk starts at the root and descends only towards `dir`, so that every ignore file on
    /// the way applies.
    fn watch_tree(state: &mut State, root: &Path, dir: &Path) -> Result<()> {
        let mut walk = ignore::WalkBuilder::new(root);
        walk.add_custom_ignore_filename(".rgignore");
        let target = dir.to_path_buf();
        walk.filter_entry(move |e| target.starts_with(e.path()) || e.path().starts_with(&target));
        for e in walk.build().flatten() {
            if !e.path().starts_with(dir) {
                continue;
            }
            if e.file_type().is_some_and(|t| t.is_dir()) {
                match inotify::add_watch(&state.fd, e.path(), flags()) {
                    Ok(wd) => {
                        state.dirs.insert(wd, relative(root, e.path()).unwrap_or_default());
                    }
                    Err(rustix::io::Errno::NOSPC) => {
                        anyhow::bail!("the inotify watch limit is reached (raise fs.inotify.max_user_watches)")
                    }
                    Err(_) => {} // removed meanwhile
                }
            }
        }
        Ok(())
    }

    impl Imp {
        pub fn start(root: &Path, sink: Arc<dyn Sink>) -> Result<Imp> {
            let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).context("inotify_init")?;
            let mut state = State { fd, dirs: HashMap::new() };
            watch_tree(&mut state, root, root)?;
            let state = Arc::new(Mutex::new(state));
            let imp = Imp { root: root.to_path_buf(), sink, state };
            // Drain in the background as well, so that the kernel queue does not overflow
            // between queries.
            let weak: Weak<Mutex<State>> = Arc::downgrade(&imp.state);
            let (root2, sink2) = (imp.root.clone(), imp.sink.clone());
            std::thread::spawn(move || loop {
                let Some(state) = weak.upgrade() else { return };
                let fd = state.lock().unwrap().fd.as_fd().try_clone_to_owned();
                drop(state);
                let Ok(fd) = fd else { return };
                let mut pfd = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
                let timeout = rustix::event::Timespec { tv_sec: 0, tv_nsec: 200_000_000 };
                let _ = rustix::event::poll(&mut pfd, Some(&timeout));
                let Some(state) = weak.upgrade() else { return };
                drain(&state, &root2, &*sink2);
            });
            Ok(imp)
        }

        pub fn sync(&self) {
            drain(&self.state, &self.root, &*self.sink);
        }
    }

    fn drain(state: &Mutex<State>, root: &Path, sink: &dyn Sink) {
        let mut st = state.lock().unwrap();
        let mut changed = Vec::new();
        let mut rescan = false;
        let mut new_dirs = Vec::new();
        let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 64 * 1024];
        loop {
            let State { fd, dirs } = &mut *st;
            let mut reader = inotify::Reader::new(fd.as_fd(), &mut buf);
            let mut got = false;
            loop {
                let event = match reader.next() {
                    Ok(e) => e,
                    Err(_) => break, // EAGAIN: the queue is empty
                };
                got = true;
                let flags = event.events();
                if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                    rescan = true;
                    continue;
                }
                let Some(dir) = dirs.get(&event.wd()).cloned() else { continue };
                if flags.contains(ReadFlags::IGNORED) {
                    dirs.remove(&event.wd());
                    continue;
                }
                let path = match event.file_name() {
                    Some(name) => dir.join(std::ffi::OsStr::from_bytes(name.to_bytes())),
                    None => dir.clone(),
                };
                if flags.contains(ReadFlags::ISDIR) && flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO) {
                    new_dirs.push(path.clone());
                }
                let name = path.file_name().map(|n| n.as_bytes()).unwrap_or(b"");
                let rules = matches!(name, b".gitignore" | b".ignore" | b".rgignore");
                if flags.intersects(ReadFlags::CLOSE_WRITE)
                    && !flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO)
                    && !rules
                {
                    continue; // a content change: only ignore files change the index
                }
                if rules {
                    // Folders below may now be entered by rg (or no longer).
                    new_dirs.push(path.parent().map(Path::to_path_buf).unwrap_or_default());
                }
                changed.push(path);
            }
            if !got {
                break;
            }
        }
        for d in new_dirs {
            let abs = root.join(&d);
            if abs.is_dir() && watch_tree(&mut st, root, &abs).is_err() {
                rescan = true;
            }
        }
        // The lock is held until the sink has applied the changes: a `sync` that finds the
        // queue empty must not return while a background drain still applies earlier events.
        if !changed.is_empty() || rescan {
            sink.changed(changed, rescan);
        }
        drop(st);
    }
}
