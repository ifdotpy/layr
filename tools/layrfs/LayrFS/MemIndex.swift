// In-memory index of the backing tree (config index=memory).
// It models the planned design: the base tree lives in daemon memory, so metadata calls need no syscall.
// No FSKit dependency, so selftest/ can compile and compare it with the disk backend.

import Darwin
import Foundation

/// A reader-writer lock (lookups and enumerations share it; updates take it exclusively).
final class RWLock {
    private let l = UnsafeMutablePointer<pthread_rwlock_t>.allocate(capacity: 1)
    init() { pthread_rwlock_init(l, nil) }
    deinit { pthread_rwlock_destroy(l); l.deallocate() }
    @inline(__always) func read<R>(_ body: () throws -> R) rethrows -> R {
        pthread_rwlock_rdlock(l); defer { pthread_rwlock_unlock(l) }
        return try body()
    }
    @inline(__always) func write<R>(_ body: () throws -> R) rethrows -> R {
        pthread_rwlock_wrlock(l); defer { pthread_rwlock_unlock(l) }
        return try body()
    }
}

final class MemIndex {
    struct Child {
        var name: [UInt8]   // on-disk name
        var ino: UInt64
    }

    struct Node {
        var entry: Entry
        var target: [UInt8]? = nil                 // symlink contents
        var children: [String: Child]? = nil       // directories only; key = lookup key of the name
        var sorted: [Child]? = nil                 // cache for enumeration, nil when stale
        var gen: UInt64 = 1                        // directory version, used as the enumeration verifier
    }

    struct BuildStats {
        var entries = 0, dirs = 0, files = 0, symlinks = 0, others = 0, errors = 0
        var ms = 0.0
        var firstErrors: [String] = []
        var json: [String: Any] {
            ["entries": entries, "dirs": dirs, "files": files, "symlinks": symlinks, "others": others,
             "errors": errors, "build_ms": ms, "first_errors": firstErrors]
        }
    }

    let rootIno: UInt64
    let caseSensitive: Bool
    private let lock = RWLock()
    private var nodes: [UInt64: Node] = [:]
    private var cachedStatfs = Darwin.statfs()
    private var statfsTime: UInt64 = 0

    /// Walks the backing tree once with getattrlistbulk (same values as Backend.stat).
    init(backend: Backend, stats: inout BuildStats) throws {
        let t0 = clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
        caseSensitive = pathconf(backend.rootPath, _PC_CASE_SENSITIVE) == 1
        let root = try backend.stat(".")
        rootIno = root.fileID
        nodes.reserveCapacity(1 << 16)
        nodes[root.fileID] = Node(entry: root, children: [:])
        stats.entries = 1; stats.dirs = 1

        var queue: [(ino: UInt64, path: String)] = [(root.fileID, ".")]
        var qi = 0
        while qi < queue.count {
            let (dirIno, dirPath) = queue[qi]
            qi += 1
            let list: [Entry]
            do { list = try backend.list(dirPath) } catch {
                stats.errors += 1
                if stats.firstErrors.count < 20 { stats.firstErrors.append("\(error)") }
                continue
            }
            var kids: [String: Child] = [:]
            kids.reserveCapacity(list.count)
            for e in list {
                let childPath = dirPath == "." ? e.nameString : dirPath + "/" + e.nameString
                kids[key(e.name)] = Child(name: e.name, ino: e.fileID)
                if nodes[e.fileID] != nil { continue }  // second hard link to a known file
                var n = Node(entry: e)
                switch e.vtype {
                case .dir:
                    n.children = [:]
                    queue.append((e.fileID, childPath))
                    stats.dirs += 1
                case .lnk:
                    do { n.target = try backend.readlink(childPath) } catch {
                        stats.errors += 1
                        if stats.firstErrors.count < 20 { stats.firstErrors.append("\(error)") }
                        n.target = []
                    }
                    stats.symlinks += 1
                case .reg: stats.files += 1
                default: stats.others += 1
                }
                nodes[e.fileID] = n
                stats.entries += 1
            }
            nodes[dirIno]!.children = kids
        }
        if let s = try? backend.statfs() { cachedStatfs = s; statfsTime = clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
        stats.ms = Double(clock_gettime_nsec_np(CLOCK_UPTIME_RAW) - t0) / 1e6
    }

    // MARK: name keys

    /// Lookup key: the name itself on a case-sensitive volume; otherwise a case-folded, decomposed form
    /// (APFS default volumes ignore case and Unicode normalization).
    func key(_ name: [UInt8]) -> String {
        if caseSensitive { return String(decoding: name, as: UTF8.self).decomposedStringWithCanonicalMapping }
        if name.allSatisfy({ $0 < 0x80 }) {
            return String(decoding: name.map { ($0 >= 0x41 && $0 <= 0x5A) ? $0 + 0x20 : $0 }, as: UTF8.self)
        }
        return String(decoding: name, as: UTF8.self).decomposedStringWithCanonicalMapping
            .folding(options: [.caseInsensitive], locale: nil)
    }

    func key(_ name: String) -> String { key(Array(name.utf8)) }

    // MARK: reads (no syscalls)

    var count: Int { lock.read { nodes.count } }

    func entry(_ ino: UInt64) -> Entry? { lock.read { nodes[ino]?.entry } }

    func symlinkTarget(_ ino: UInt64) -> [UInt8]? { lock.read { nodes[ino]?.target } }

    /// The child entry, with `name` set to the stored on-disk name.
    func lookup(dir: UInt64, name: String) throws -> Entry {
        let k = key(name)
        return try lock.read {
            guard let d = nodes[dir] else { throw PosixError(code: ENOENT, op: "index: no dir") }
            guard let kids = d.children else { throw PosixError(code: ENOTDIR, op: "index lookup") }
            guard let c = kids[k], var e = nodes[c.ino]?.entry else { throw PosixError(code: ENOENT, op: "index lookup") }
            e.name = c.name
            return e
        }
    }

    /// Sorted children and the directory version. The sorted list is cached until the next change.
    func children(_ dir: UInt64) throws -> (gen: UInt64, kids: [Child]) {
        if let r = try lock.read({ () throws -> (UInt64, [Child])? in
            guard let d = nodes[dir] else { throw PosixError(code: ENOENT, op: "index: no dir") }
            guard d.children != nil else { throw PosixError(code: ENOTDIR, op: "index enumerate") }
            return d.sorted.map { (d.gen, $0) }
        }) { return r }
        return try lock.write {
            guard var d = nodes[dir], let kids = d.children else { throw PosixError(code: ENOENT, op: "index: no dir") }
            if let s = d.sorted { return (d.gen, s) }
            let s = kids.values.sorted { $0.name.lexicographicallyPrecedes($1.name) }
            d.sorted = s
            nodes[dir] = d
            return (d.gen, s)
        }
    }

    /// Entries for a slice of children, under one lock acquisition. The name is the child's name.
    func entries(_ kids: ArraySlice<Child>) -> [Entry?] {
        lock.read {
            kids.map { c in
                guard var e = nodes[c.ino]?.entry else { return nil }
                e.name = c.name
                return e
            }
        }
    }

    /// Cached statfs; refreshed at most once per second, and only by mutations.
    var statfs: Darwin.statfs { lock.read { cachedStatfs } }

    // MARK: updates (after the disk operation succeeded; syscalls are allowed here)

    private func touchDirLocked(_ dir: UInt64) {
        nodes[dir]?.sorted = nil
        nodes[dir]?.gen &+= 1
        if nodes[dir]?.gen == 0 { nodes[dir]?.gen = 1 }
    }

    private func refreshStatfs(_ backend: Backend) {
        let now = clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
        if now - lock.read({ statfsTime }) < 1_000_000_000 { return }
        if let s = try? backend.statfs() { lock.write { cachedStatfs = s; statfsTime = now } }
    }

    /// Replaces the attributes of a known object.
    func update(_ e: Entry) {
        lock.write {
            guard var n = nodes[e.fileID] else { return }
            var e = e
            if e.name.isEmpty { e.name = n.entry.name }
            n.entry = e
            nodes[e.fileID] = n
        }
    }

    /// Re-reads the attributes of the object at `path` (or of `fd`) and stores them.
    @discardableResult
    func refresh(path: String, backend: Backend) throws -> Entry {
        let e = try backend.stat(path)
        update(e)
        return e
    }

    /// Removes one name from a directory. Drops the node when its last link goes (with its subtree for a directory).
    private func removeNameLocked(dir: UInt64, key k: String) -> UInt64? {
        guard let c = nodes[dir]?.children?[k] else { return nil }
        nodes[dir]!.children![k] = nil
        touchDirLocked(dir)
        guard var n = nodes[c.ino] else { return c.ino }
        if n.children != nil || n.entry.linkCount <= 1 {
            dropLocked(c.ino)
        } else {
            n.entry.linkCount -= 1
            nodes[c.ino] = n
        }
        return c.ino
    }

    private func dropLocked(_ ino: UInt64) {
        guard let n = nodes.removeValue(forKey: ino) else { return }
        if let kids = n.children {
            for c in kids.values { dropLocked(c.ino) }
        }
    }

    /// After create, mkdir, symlink, or link of `name` in the directory `dir` at `dirPath`.
    @discardableResult
    func didCreate(dir: UInt64, dirPath: String, name: String, backend: Backend) throws -> (child: Entry, dir: Entry) {
        let childPath = dirPath == "." ? name : dirPath + "/" + name
        var e = try backend.stat(childPath)
        let target = e.vtype == .lnk ? try backend.readlink(childPath) : nil
        let d = try backend.stat(dirPath)
        if e.name.isEmpty { e.name = Array(name.utf8) }
        lock.write {
            nodes[dir]?.children?[key(e.name)] = Child(name: e.name, ino: e.fileID)
            touchDirLocked(dir)
            if var n = nodes[e.fileID] {
                n.entry = e                      // new hard link: keep the subtree, refresh attributes
                nodes[e.fileID] = n
            } else {
                nodes[e.fileID] = Node(entry: e, target: target, children: e.vtype == .dir ? [:] : nil)
            }
            if var dn = nodes[dir] { var de = d; de.name = dn.entry.name; dn.entry = de; nodes[dir] = dn }
        }
        refreshStatfs(backend)
        return (e, d)
    }

    /// After unlink or rmdir of `name` in `dir`.
    @discardableResult
    func didRemove(dir: UInt64, dirPath: String, name: String, backend: Backend) throws -> Entry {
        let d = try backend.stat(dirPath)
        lock.write {
            _ = removeNameLocked(dir: dir, key: key(name))
            if var dn = nodes[dir] { var de = d; de.name = dn.entry.name; dn.entry = de; nodes[dir] = dn }
        }
        refreshStatfs(backend)
        return d
    }

    /// After rename(2) from `srcName` in `src` to `dstName` in `dst`.
    func didRename(src: UInt64, srcPath: String, srcName: String, dst: UInt64, dstPath: String, dstName: String,
                   backend: Backend) throws
    {
        let newPath = dstPath == "." ? dstName : dstPath + "/" + dstName
        var e = try backend.stat(newPath)
        let sd = try backend.stat(srcPath)
        let dd = src == dst ? sd : try backend.stat(dstPath)
        if e.name.isEmpty { e.name = Array(dstName.utf8) }
        lock.write {
            let sk = key(srcName), dk = key(e.name)
            guard let moved = nodes[src]?.children?[sk] else { return }
            nodes[src]!.children![sk] = nil
            touchDirLocked(src)
            if let over = nodes[dst]?.children?[dk], over.ino != moved.ino {
                _ = removeNameLocked(dir: dst, key: dk)
            }
            nodes[dst]?.children?[dk] = Child(name: e.name, ino: moved.ino)
            touchDirLocked(dst)
            if var n = nodes[moved.ino] { n.entry = e; nodes[moved.ino] = n }
            for (ino, de) in [(src, sd), (dst, dd)] {
                if var dn = nodes[ino] { var x = de; x.name = dn.entry.name; dn.entry = x; nodes[ino] = dn }
            }
        }
    }
}
