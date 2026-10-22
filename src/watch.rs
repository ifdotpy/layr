//! FSEvents on APFS: the paths that changed below a folder since a stored event ID, read from
//! the volume event log by a short-lived process (no daemon).
//!
//! The log can be off (disk images, `.fseventsd/no_log`), can drop events, and delivers events
//! with a delay. Two marker files on the same volume guard against all three:
//! - `mark` writes a marker right after it reads the current event ID. A later read must see
//!   that marker, or the log does not cover the time since then.
//! - `changes` writes a second marker after its stream starts and waits for it. Events come in
//!   ID order, so every change before the marker has arrived.
//! Any doubt returns `None`; the caller then reads the whole folder.
use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type CFIndex = isize;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

const UTF8: u32 = 0x0800_0100;
const FILE_EVENTS: u32 = 0x10;
const NO_DEFER: u32 = 0x02;
// Flags that make the stream incomplete for our purpose.
const MUST_SCAN: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const HISTORY_DONE: u32 = 0x10;
pub const ITEM_CREATED: u32 = 0x100;
pub const ITEM_REMOVED: u32 = 0x200;
pub const ITEM_RENAMED: u32 = 0x800;
pub const ITEM_IS_DIR: u32 = 0x20000;
pub const ITEM_IS_HARDLINK: u32 = 0x100000;

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventStreamCreate(
        alloc: CFAllocatorRef,
        callback: Callback,
        context: *mut FSEventStreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamFlushSync(stream: FSEventStreamRef);
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFArrayCreate(alloc: CFAllocatorRef, values: *const *const c_void, n: CFIndex, callbacks: *const c_void) -> CFArrayRef;
    fn CFRelease(obj: *const c_void);
    static kCFTypeArrayCallBacks: c_void;
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(obj: *mut c_void);
}

#[derive(Default)]
struct Seen {
    paths: Vec<(String, u32, u64)>,
    incomplete: bool,
}

extern "C" fn on_events(
    _s: FSEventStreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    // SAFETY: FSEvents passes `n` C strings and flags; `info` is the Mutex that `changes` owns
    // until the stream is released.
    unsafe {
        let seen = &*(info as *const Mutex<Seen>);
        let paths = paths as *const *const c_char;
        let mut s = seen.lock().unwrap();
        for i in 0..n {
            let f = *flags.add(i);
            if f & (MUST_SCAN | USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED | ROOT_CHANGED | MOUNT | UNMOUNT) != 0 {
                s.incomplete = true;
            }
            if f & HISTORY_DONE != 0 {
                continue;
            }
            let p = std::ffi::CStr::from_ptr(*paths.add(i)).to_string_lossy().into_owned();
            s.paths.push((p, f, *ids.add(i)));
        }
    }
}

/// The current event ID of the system.
pub fn current_id() -> u64 {
    // SAFETY: no arguments; the call only reads a counter.
    unsafe { FSEventsGetCurrentEventId() }
}

fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, crate::ids::new_id())
}

/// Read the current event ID, then change the marker file. `changes` later needs to see that
/// marker change to trust the log since the returned ID.
pub fn mark(marker: &Path) -> std::io::Result<u64> {
    let id = current_id();
    touch(marker)?;
    Ok(id)
}

/// Paths below `root` that changed since `since`, relative to `root` ("" for the root itself),
/// with their event flags. `marker` is the file that `mark` changed; `cookie` is a file in the
/// same folder that this call changes. `None` when the log does not prove a complete answer.
pub fn changes(root: &Path, marker: &Path, cookie: &Path, since: u64) -> Option<BTreeMap<String, u32>> {
    let _t = crate::trace::span("FSEvents changes");
    let root = std::fs::canonicalize(root).ok()?;
    let marker = std::fs::canonicalize(marker).ok()?;
    let cookie_dir = std::fs::canonicalize(cookie.parent()?).ok()?;
    let cookie = cookie_dir.join(cookie.file_name()?);
    let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Seen::default()));
    let roots = [root.clone(), cookie_dir.clone()];
    // SAFETY: every Core Foundation object is released below; the stream keeps a pointer to
    // `seen`, which outlives the stream because the stream is released before return.
    unsafe {
        let strings: Vec<CFStringRef> = roots
            .iter()
            .filter_map(|r| CString::new(r.to_string_lossy().as_bytes()).ok())
            .map(|c| CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8))
            .collect();
        if strings.len() != 2 || strings.iter().any(|s| s.is_null()) {
            return None;
        }
        let array = CFArrayCreate(std::ptr::null(), strings.as_ptr(), 2, &kCFTypeArrayCallBacks as *const c_void);
        for s in &strings {
            CFRelease(*s);
        }
        let mut ctx = FSEventStreamContext {
            version: 0,
            info: Arc::as_ptr(&seen) as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let stream = FSEventStreamCreate(std::ptr::null(), on_events, &mut ctx, array, since, 0.0, FILE_EVENTS | NO_DEFER);
        CFRelease(array);
        if stream.is_null() {
            return None;
        }
        let label = CString::new("layr.fsevents").unwrap();
        let queue = dispatch_queue_create(label.as_ptr(), std::ptr::null());
        FSEventStreamSetDispatchQueue(stream, queue);
        let started = FSEventStreamStart(stream) != 0;
        let mut ok = started && touch(&cookie).is_ok();
        let deadline = Instant::now() + Duration::from_secs(3);
        let cookie_s = cookie.to_string_lossy().into_owned();
        while ok {
            FSEventStreamFlushSync(stream);
            if seen.lock().unwrap().paths.iter().any(|(p, _, id)| *p == cookie_s && *id > since) {
                break;
            }
            if Instant::now() > deadline {
                ok = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if started {
            FSEventStreamStop(stream);
        }
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
        dispatch_release(queue);
        if !ok {
            return None;
        }
    }
    let seen = seen.lock().unwrap();
    let marker_s = marker.to_string_lossy();
    if seen.incomplete || !seen.paths.iter().any(|(p, _, id)| *p == marker_s && *id > since) {
        return None;
    }
    let prefix = format!("{}/", root.to_string_lossy());
    let root_s = root.to_string_lossy().into_owned();
    let mut out = BTreeMap::new();
    for (p, flags, _) in seen.paths.iter() {
        if p.starts_with(&cookie_dir.to_string_lossy().into_owned()) && !p.starts_with(&prefix) {
            continue;
        }
        if *p == root_s {
            *out.entry(String::new()).or_insert(0) |= *flags;
        } else if let Some(rel) = p.strip_prefix(&prefix) {
            *out.entry(rel.to_string()).or_insert(0) |= *flags;
        } else if root_s.starts_with(&format!("{p}/")) {
            // A change of a folder above the root (a rename) hides what happened below.
            return None;
        }
    }
    Some(out)
}

/// The marker and cookie files of a line, in the project's private folder.
pub fn files(meta: &Path, line: &str) -> (PathBuf, PathBuf) {
    let dir = meta.join("watch");
    (dir.join(format!("{line}.marker")), dir.join(format!("{line}.cookie")))
}
