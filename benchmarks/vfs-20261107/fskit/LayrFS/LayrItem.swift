// FSItem subclass and the item table of the passthrough volume.

import Darwin
import Foundation
import FSKit

/// A directory listing kept between enumerateDirectory calls of one enumeration.
struct DirSnapshot {
    let verifier: UInt64
    let withAttributes: Bool
    let entries: [Entry]
}

/// One backing object. The path is not stored: it is computed from `parent` and `name`,
/// so a directory rename updates every descendant at once.
///
/// Fields marked (tree) are guarded by `ItemTable.lock`.
/// Fields marked (io) are guarded by `ioLock`.
final class LayrItem: FSItem {
    let ino: UInt64
    let type: FSItem.ItemType

    var parent: LayrItem?          // (tree) strong: a live child pins its parent
    var name: String               // (tree)
    var childCount = 0             // (tree) number of table-resident children
    var inTable = false            // (tree)
    var kernelReclaimed = false    // (tree)
    var unlinked = false           // (tree) last link removed; only the open fd is valid
    var stalePath = false          // (tree) this name was removed but other hard links remain
    var snapshot: DirSnapshot?     // (tree)

    private let ioLock = UnsafeMutablePointer<pthread_rwlock_t>.allocate(capacity: 1)
    private var fd: Int32 = -1     // (io)
    private var fdWritable = false // (io)
    private var fdBudgeted = false // (io) fd came from openCached and holds one FDBudget slot

    /// Limits descriptors that stay open only for caching (index mode reads without open/close calls).
    static let fdBudget = FDBudget()

    init(ino: UInt64, type: FSItem.ItemType, parent: LayrItem?, name: String) {
        self.ino = ino
        self.type = type
        self.parent = parent
        self.name = name
        pthread_rwlock_init(ioLock, nil)
        super.init()
    }

    deinit {
        if fd >= 0 { close(fd) }
        pthread_rwlock_destroy(ioLock)
        ioLock.deallocate()
    }

    /// Opens (or upgrades) the cached descriptor for the open modes.
    func ensureOpen(path: String, backend: Backend, write: Bool) throws {
        pthread_rwlock_wrlock(ioLock)
        defer { pthread_rwlock_unlock(ioLock) }
        if fd >= 0 && (fdWritable || !write) { return }
        var newFD: Int32
        do {
            newFD = try backend.openFile(path, flags: write ? O_RDWR : O_RDONLY)
        } catch let e as PosixError where write && e.code == EACCES {
            newFD = try backend.openFile(path, flags: O_WRONLY)  // write-only file
        }
        if fd >= 0 { close(fd) }
        if fdBudgeted { LayrItem.fdBudget.release(); fdBudgeted = false }
        fd = newFD
        fdWritable = write
    }

    /// Opens a read-only descriptor and keeps it until close or reclaim, if the budget allows.
    /// Returns false when the budget is used up (the caller then uses a temporary descriptor).
    func openCached(path: String, backend: Backend) throws -> Bool {
        pthread_rwlock_wrlock(ioLock)
        defer { pthread_rwlock_unlock(ioLock) }
        if fd >= 0 { return true }
        guard LayrItem.fdBudget.acquire() else { return false }
        do {
            fd = try backend.openFile(path, flags: O_RDONLY)
        } catch {
            LayrItem.fdBudget.release()
            throw error
        }
        fdWritable = false
        fdBudgeted = true
        return true
    }

    func closeFD() {
        pthread_rwlock_wrlock(ioLock)
        if fd >= 0 { close(fd); fd = -1; fdWritable = false }
        if fdBudgeted { LayrItem.fdBudget.release(); fdBudgeted = false }
        pthread_rwlock_unlock(ioLock)
    }

    /// Runs `body` with the cached descriptor under the shared lock, or returns nil when none fits.
    func withFD<R>(write: Bool, _ body: (Int32) throws -> R) rethrows -> R? {
        pthread_rwlock_rdlock(ioLock)
        defer { pthread_rwlock_unlock(ioLock) }
        guard fd >= 0, fdWritable || !write else { return nil }
        return try body(fd)
    }
}

/// Counts cached descriptors. The limit is set from RLIMIT_NOFILE at start.
final class FDBudget {
    private let lock = NSLock()
    private var used = 0
    private(set) var limit = 1024

    /// Raises the soft descriptor limit and keeps half of it for cached descriptors.
    func configure() {
        var rl = rlimit()
        getrlimit(RLIMIT_NOFILE, &rl)
        var maxPerProc: Int32 = 0
        var size = MemoryLayout<Int32>.size
        sysctlbyname("kern.maxfilesperproc", &maxPerProc, &size, nil, 0)
        let want = min(rl.rlim_max, rlim_t(max(maxPerProc, 1024)), 65536)
        if want > rl.rlim_cur { rl.rlim_cur = want; setrlimit(RLIMIT_NOFILE, &rl) }
        getrlimit(RLIMIT_NOFILE, &rl)
        lock.lock(); limit = max(64, Int(rl.rlim_cur) / 2); lock.unlock()
    }

    func acquire() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if used >= limit { return false }
        used += 1
        return true
    }

    func release() { lock.lock(); used -= 1; lock.unlock() }

    var json: [String: Any] { lock.lock(); defer { lock.unlock() }; return ["used": used, "limit": limit] }
}

/// The ino -> item table. One lock guards the table and all (tree) fields.
final class ItemTable {
    let lock = NSLock()
    private var byIno: [UInt64: LayrItem] = [:]
    let root: LayrItem

    init(root: LayrItem) {
        self.root = root
        byIno[root.ino] = root
        root.inTable = true
    }

    var count: Int { lock.lock(); defer { lock.unlock() }; return byIno.count }

    /// Relative path for *at calls. Caller holds `lock`.
    func pathLocked(_ item: LayrItem) throws -> String {
        if item === root { return "." }
        if item.unlinked || item.stalePath { throw PosixError(code: ESTALE, op: "path of removed item") }
        var parts: [String] = []
        var cur: LayrItem? = item
        while let c = cur, c !== root {
            parts.append(c.name)
            cur = c.parent
            if cur == nil { throw PosixError(code: ESTALE, op: "orphan item") }
        }
        return parts.reversed().joined(separator: "/")
    }

    func path(_ item: LayrItem) throws -> String {
        lock.lock(); defer { lock.unlock() }
        return try pathLocked(item)
    }

    func childPath(_ dir: LayrItem, _ name: String) throws -> String {
        lock.lock(); defer { lock.unlock() }
        let p = try pathLocked(dir)
        return p == "." ? name : p + "/" + name
    }

    /// Returns the item for `e`, creating or re-parenting it. Clears reclaim and stale marks.
    func resolve(_ e: Entry, type: FSItem.ItemType, parent: LayrItem, name: String) -> LayrItem {
        lock.lock(); defer { lock.unlock() }
        if let it = byIno[e.fileID] {
            it.kernelReclaimed = false
            it.stalePath = false
            if it !== root && (it.parent !== parent || it.name != name) {
                moveLocked(it, to: parent, name: name)
            }
            return it
        }
        let it = LayrItem(ino: e.fileID, type: type, parent: parent, name: name)
        byIno[e.fileID] = it
        it.inTable = true
        parent.childCount += 1
        return it
    }

    /// Updates parent and name after a rename or a lookup through another hard link. Caller holds `lock`.
    func moveLocked(_ it: LayrItem, to newParent: LayrItem, name: String) {
        let old = it.parent
        it.name = name
        if old === newParent { return }
        it.parent = newParent
        if it.inTable {
            newParent.childCount += 1
            if let old { old.childCount -= 1; collectLocked(old) }
        }
    }

    /// Removes `it` from the table and drops its pin on the parent chain. Caller holds `lock`.
    func detachLocked(_ it: LayrItem) {
        guard it.inTable, it !== root else { return }
        if byIno[it.ino] === it { byIno[it.ino] = nil }
        it.inTable = false
        if let p = it.parent { p.childCount -= 1; collectLocked(p) }
    }

    /// Detaches a parent the kernel already reclaimed once its last child is gone.
    private func collectLocked(_ p: LayrItem) {
        if p.kernelReclaimed && p.childCount == 0 && p !== root { detachLocked(p) }
    }

    /// Kernel reclaim. Returns true when the item left the table.
    func reclaimLocked(_ it: LayrItem) {
        it.kernelReclaimed = true
        it.snapshot = nil
        if it.childCount == 0 { detachLocked(it) }
    }
}
