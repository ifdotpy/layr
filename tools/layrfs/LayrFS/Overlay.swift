// Overlay core of a virtual line: a writable upper folder over a read-only lower tree.
// No FSKit dependency, so selftest/ can compile and run it directly.
//
// The upper folder is a Layr thin layer (docs/apfs-thin-states.md, "Layer rules"):
// - An entry in the upper folder replaces the same path of the lower tree.
// - A folder in the upper folder merges with the lower folder, unless the path is removed.
// - The removed paths (whiteouts) are a JSON array in the attribute `com.layr.removed` of the
//   upper root. A removed path hides the lower entry and all of its content.
// Layr reads the same folder as `thin::Layered::over(upper, base)` for status, diff and save.

import Darwin
import Foundation

/// Lookup keys of relative paths: the path itself on a case-sensitive volume, else a case-folded,
/// decomposed form (APFS default volumes ignore case and Unicode normalization).
struct PathKeys {
    let caseSensitive: Bool

    func key(_ path: String) -> String {
        if caseSensitive { return path.decomposedStringWithCanonicalMapping }
        let u = Array(path.utf8)
        if u.allSatisfy({ $0 < 0x80 }) {
            return String(decoding: u.map { ($0 >= 0x41 && $0 <= 0x5A) ? $0 + 0x20 : $0 }, as: UTF8.self)
        }
        return path.decomposedStringWithCanonicalMapping.folding(options: [.caseInsensitive], locale: nil)
    }
}

enum RelPath {
    static func parent(_ p: String) -> String {
        guard let i = p.lastIndex(of: "/") else { return "." }
        return String(p[..<i])
    }

    static func join(_ dir: String, _ name: String) -> String { dir == "." ? name : dir + "/" + name }

    /// The ancestors of `p`, nearest to the root first, without the root and without `p`.
    static func ancestors(_ p: String) -> [String] {
        var out: [String] = []
        var i = p.startIndex
        while let j = p[i...].firstIndex(of: "/") {
            out.append(String(p[..<j]))
            i = p.index(after: j)
        }
        return out
    }
}

/// Flags that must not reach the merged view: Layr seals every entry of a saved state with
/// UF_IMMUTABLE, and the kernel would then refuse writes and removes of those paths.
let lockFlags = UInt32(UF_IMMUTABLE | UF_APPEND | SF_IMMUTABLE | SF_APPEND)

/// The read-only lower tree, read on demand: one getattrlistbulk call per directory, kept in
/// memory. The lower tree never changes while it is mounted, so nothing is invalidated.
final class LowerIndex {
    struct Dir {
        var entries: [Entry]            // sorted by name bytes
        var byKey: [String: Int]        // folded name -> index in entries
    }

    struct Counts {
        var dirs = 0, entries = 0, loadNs: UInt64 = 0
        var json: [String: Any] { ["dirs_loaded": dirs, "entries_loaded": entries, "load_ms": Double(loadNs) / 1e6] }
    }

    let backend: Backend
    let keys: PathKeys
    let root: Entry
    private let lock = RWLock()
    private var dirs: [String: Dir] = [:]          // folded directory path -> listing
    private var targets: [String: [UInt8]] = [:]   // folded path -> symlink target
    private(set) var counts = Counts()

    init(backend: Backend, keys: PathKeys) throws {
        self.backend = backend
        self.keys = keys
        var r = try backend.stat(".")
        r.flags &= ~lockFlags
        root = r
    }

    /// The listing of the directory at `path`; nil when it is no directory of the lower tree.
    func listing(_ path: String) -> Dir? {
        let k = path == "." ? "." : keys.key(path)
        if let d = lock.read({ dirs[k] }) { return d }
        let t0 = nowNs()
        guard let list = try? backend.list(path) else { return nil }
        var entries = list.map { e -> Entry in var e = e; e.flags &= ~lockFlags; return e }
        entries.sort { $0.name.lexicographicallyPrecedes($1.name) }
        var byKey: [String: Int] = [:]
        byKey.reserveCapacity(entries.count)
        for (i, e) in entries.enumerated() { byKey[keys.key(e.nameString)] = i }
        let d = Dir(entries: entries, byKey: byKey)
        return lock.write {
            if let other = dirs[k] { return other }
            dirs[k] = d
            counts.dirs += 1
            counts.entries += entries.count
            counts.loadNs += nowNs() - t0
            return d
        }
    }

    /// The lower entry at `path` ("." is the root), nil when the lower tree has none.
    func entry(_ path: String) -> Entry? {
        if path == "." { return root }
        let parent = RelPath.parent(path)
        guard let d = listing(parent) else { return nil }
        let name: String
        if let s = path.lastIndex(of: "/") { name = String(path[path.index(after: s)...]) } else { name = path }
        guard let i = d.byKey[keys.key(name)] else { return nil }
        return d.entries[i]
    }

    func readlink(_ path: String) throws -> [UInt8] {
        let k = keys.key(path)
        if let t = lock.read({ targets[k] }) { return t }
        let t = try backend.readlink(path)
        lock.write { targets[k] = t }
        return t
    }

    /// Reads every directory of the lower tree (config prefetch=1).
    func loadAll() {
        var queue = ["."]
        var i = 0
        while i < queue.count {
            let p = queue[i]
            i += 1
            guard let d = listing(p) else { continue }
            for e in d.entries where e.vtype == .dir { queue.append(RelPath.join(p, e.nameString)) }
        }
    }

    var countsJSON: [String: Any] { lock.read { counts.json } }
}

/// Opens files for LayrItem descriptors: the passthrough Backend, or the overlay.
protocol FileOpener: AnyObject {
    func openFile(_ rel: String, flags: Int32, mode: mode_t) throws -> Int32
}

extension Backend: FileOpener {}

/// The merged view of an upper folder over a lower tree. Every path is relative ("." is the root).
/// One recursive lock serializes the changes; reads take it only to check the in-memory sets.
final class Overlay: FileOpener {
    static let removedAttr = "com.layr.removed"
    static let hiddenAttrPrefix = "com.layr."
    static let quarantineAttr = "com.apple.quarantine"
    /// Volume folders that macOS services make at the root of a mount (FSEvents log, Spotlight,
    /// Trash). They are refused there: they are no content of the line.
    static let reservedRootNames: Set<String> = [".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems",
                                                 ".DocumentRevisions-V100"]

    let lower: Backend
    let upper: Backend
    let lowerIndex: LowerIndex
    let keys: PathKeys
    /// The virtual root ID: the inode of the lower root.
    let rootIno: UInt64

    private let mu = NSRecursiveLock()
    private var upperTypes: [String: VType] = [:]       // folded path -> type of each upper entry
    private var removed: [String: String] = [:]         // folded path -> stored path (whiteouts)
    private var alias: [UInt64: UInt64] = [:]           // upper inode -> virtual inode
    private var mutations: UInt64 = 1
    private(set) var copyUps: UInt64 = 0
    private(set) var quarantineKept: UInt64 = 0

    init(lowerPath: String, upperPath: String) throws {
        lower = try Backend(rootPath: lowerPath)
        upper = try Backend(rootPath: upperPath)
        var ls = Darwin.stat(), us = Darwin.stat()
        try check(Darwin.fstat(lower.rootFD, &ls), "fstat lower")
        try check(Darwin.fstat(upper.rootFD, &us), "fstat upper")
        // Virtual IDs are inode numbers of both folders: they must come from one volume.
        if ls.st_dev != us.st_dev { throw PosixError(code: EXDEV, op: "lower and upper are on different volumes") }
        keys = PathKeys(caseSensitive: pathconf(lowerPath, _PC_CASE_SENSITIVE) == 1)
        lowerIndex = try LowerIndex(backend: lower, keys: keys)
        rootIno = lowerIndex.root.fileID
        try loadState()
    }

    /// Reads the whiteouts and the upper entries; an upper entry over a visible lower entry
    /// keeps the lower inode as its ID.
    private func loadState() throws {
        if let data = try? upper.getXattr(".", name: Overlay.removedAttr), !data.isEmpty {
            guard let list = try JSONSerialization.jsonObject(with: data) as? [String] else {
                throw PosixError(code: EINVAL, op: "\(Overlay.removedAttr) is not a JSON array of paths")
            }
            for p in list where !p.isEmpty { removed[keys.key(p)] = p }
        }
        let rootUpper = try upper.stat(".")
        alias[rootUpper.fileID] = rootIno
        var queue = ["."]
        var i = 0
        while i < queue.count {
            let dir = queue[i]
            i += 1
            for e in try upper.list(dir) {
                let p = RelPath.join(dir, e.nameString)
                upperTypes[keys.key(p)] = e.vtype
                if let l = lowerEntryLocked(p) { alias[e.fileID] = l.fileID }
                if e.vtype == .dir { queue.append(p) }
            }
        }
    }

    // MARK: state checks (caller holds mu, or reads a value that only the caller changes)

    /// True when no whiteout hides `path` or one of its ancestors.
    private func lowerVisibleLocked(_ path: String) -> Bool {
        if path == "." || removed.isEmpty { return true }
        if removed[keys.key(path)] != nil { return false }
        for a in RelPath.ancestors(path) where removed[keys.key(a)] != nil { return false }
        return true
    }

    private func lowerEntryLocked(_ path: String) -> Entry? {
        guard lowerVisibleLocked(path) else { return nil }
        return lowerIndex.entry(path)
    }

    private func upperTypeLocked(_ path: String) -> VType? {
        path == "." ? .dir : upperTypes[keys.key(path)]
    }

    private func vino(_ upperIno: UInt64) -> UInt64 { alias[upperIno] ?? upperIno }

    private func fromUpper(_ e: Entry) -> Entry {
        var e = e
        e.fileID = vino(e.fileID)
        e.flags &= ~lockFlags
        return e
    }

    var generation: UInt64 { mu.lock(); defer { mu.unlock() }; return mutations }

    func inUpper(_ path: String) -> Bool { mu.lock(); defer { mu.unlock() }; return upperTypeLocked(path) != nil }

    /// The whiteouts, as stored paths.
    var whiteouts: [String] { mu.lock(); defer { mu.unlock() }; return removed.values.sorted() }

    var upperCount: Int { mu.lock(); defer { mu.unlock() }; return upperTypes.count }

    // MARK: reads

    /// Attributes of the merged entry at `path`, with the virtual ID.
    func stat(_ path: String) throws -> Entry {
        mu.lock()
        defer { mu.unlock() }
        return try statLocked(path)
    }

    private func statLocked(_ path: String) throws -> Entry {
        if path == "." {
            var e = try upper.stat(".")
            e.fileID = rootIno
            e.flags &= ~lockFlags
            return e
        }
        if upperTypeLocked(path) != nil { return fromUpper(try upper.stat(path)) }
        if let e = lowerEntryLocked(path) { return e }
        throw PosixError(code: ENOENT, op: "overlay stat \(path)")
    }

    /// Attributes of an open descriptor of the merged entry `ino`.
    func fstat(_ fd: Int32, ino: UInt64) throws -> Entry {
        var e = try upper.fstat(fd)
        e.fileID = ino
        e.flags &= ~lockFlags
        return e
    }

    /// The merged listing of the directory at `path`, sorted by name.
    func list(_ path: String) throws -> [Entry] {
        mu.lock()
        defer { mu.unlock() }
        return try listLocked(path)
    }

    private func listLocked(_ path: String) throws -> [Entry] {
        var out: [Entry] = []
        var seen = Set<String>()
        let ut = upperTypeLocked(path)
        let lowerDir = lowerEntryLocked(path).flatMap { $0.vtype == .dir ? $0 : nil }
        if ut == nil && lowerDir == nil { throw PosixError(code: ENOENT, op: "overlay list \(path)") }
        if let ut, ut != .dir { throw PosixError(code: ENOTDIR, op: "overlay list \(path)") }
        if ut == .dir {
            for e in try upper.list(path) {
                seen.insert(keys.key(e.nameString))
                out.append(fromUpper(e))
            }
        }
        if lowerDir != nil, let d = lowerIndex.listing(path) {
            for e in d.entries {
                let k = keys.key(e.nameString)
                if seen.contains(k) { continue }
                if !removed.isEmpty && removed[keys.key(RelPath.join(path, e.nameString))] != nil { continue }
                out.append(e)
            }
        }
        out.sort { $0.name.lexicographicallyPrecedes($1.name) }
        return out
    }

    func readlink(_ path: String) throws -> [UInt8] {
        mu.lock()
        let up = upperTypeLocked(path) != nil
        let low = up ? nil : lowerEntryLocked(path)
        mu.unlock()
        if up { return try upper.readlink(path) }
        guard low != nil else { throw PosixError(code: ENOENT, op: "overlay readlink \(path)") }
        return try lowerIndex.readlink(path)
    }

    /// Opens the merged file. Any write flag copies the file up first.
    func openFile(_ rel: String, flags: Int32, mode: mode_t) throws -> Int32 {
        if flags & (O_WRONLY | O_RDWR | O_TRUNC | O_APPEND) != 0 {
            try copyUp(rel)
            return try upper.openFile(rel, flags: flags, mode: mode)
        }
        mu.lock()
        let up = upperTypeLocked(rel) != nil
        let low = up ? nil : lowerEntryLocked(rel)
        mu.unlock()
        if up { return try upper.openFile(rel, flags: flags, mode: mode) }
        guard low != nil else { throw PosixError(code: ENOENT, op: "overlay open \(rel)") }
        return try lower.openFile(rel, flags: flags, mode: mode)
    }

    /// True for the quarantine mark that the sandbox of this extension puts on the files it makes.
    static func ownQuarantine(_ value: Data) -> Bool { String(decoding: value, as: UTF8.self).contains(";LayrFS;") }

    func getXattr(_ path: String, name: String) throws -> Data {
        if name.hasPrefix(Overlay.hiddenAttrPrefix) { throw PosixError(code: ENOATTR, op: "getxattr \(name)") }
        let v = try (inUpper(path) ? upper : lower).getXattr(path, name: name)
        if name == Overlay.quarantineAttr && Overlay.ownQuarantine(v) { throw PosixError(code: ENOATTR, op: "getxattr \(name)") }
        return v
    }

    func listXattrs(_ path: String) throws -> [[UInt8]] {
        let b = inUpper(path) ? upper : lower
        let names = try b.listXattrs(path)
        let hidden = Array(Overlay.hiddenAttrPrefix.utf8), quarantine = Array(Overlay.quarantineAttr.utf8)
        return names.filter { n in
            if n.starts(with: hidden) { return false }
            if n == quarantine, let v = try? b.getXattr(path, name: Overlay.quarantineAttr), Overlay.ownQuarantine(v) { return false }
            return true
        }
    }

    /// Removes the quarantine mark that the sandbox put on an entry this extension made.
    private func dequarantineLocked(_ path: String) {
        guard let v = try? upper.getXattr(path, name: Overlay.quarantineAttr), Overlay.ownQuarantine(v) else { return }
        do { try upper.removeXattr(path, name: Overlay.quarantineAttr) } catch { quarantineKept += 1 }
    }

    func statfs() throws -> Darwin.statfs { try upper.statfs() }

    // MARK: changes

    /// Makes the parent folders of `dir` and `dir` itself upper folders, with the modes of the
    /// lower folders. An upper folder over a lower folder keeps the lower inode as its ID.
    private func ensureUpperDirLocked(_ dir: String) throws {
        if dir == "." { return }
        if let t = upperTypeLocked(dir) {
            if t != .dir { throw PosixError(code: ENOTDIR, op: "overlay dir \(dir)") }
            return
        }
        guard let l = lowerEntryLocked(dir) else { throw PosixError(code: ENOENT, op: "overlay dir \(dir)") }
        if l.vtype != .dir { throw PosixError(code: ENOTDIR, op: "overlay dir \(dir)") }
        try ensureUpperDirLocked(RelPath.parent(dir))
        try upper.mkdir(dir, mode: mode_t(l.perm | 0o700))
        dequarantineLocked(dir)
        if l.perm & 0o700 != 0o700 { try upper.chmod(dir, mode: mode_t(l.perm)) }
        try? upper.utimes(dir, atime: l.accessTime, mtime: l.modifyTime)
        let u = try upper.stat(dir)
        alias[u.fileID] = l.fileID
        upperTypes[keys.key(dir)] = .dir
    }

    /// Copies the lower entry at `path` into the upper folder (a clone for a file or a link, a new
    /// folder with the lower mode for a folder). Returns true when it copied.
    @discardableResult
    func copyUp(_ path: String) throws -> Bool {
        mu.lock()
        defer { mu.unlock() }
        return try copyUpLocked(path)
    }

    private func copyUpLocked(_ path: String) throws -> Bool {
        if upperTypeLocked(path) != nil { return false }
        guard let l = lowerEntryLocked(path) else { throw PosixError(code: ENOENT, op: "overlay copy-up \(path)") }
        if l.vtype == .dir {
            try ensureUpperDirLocked(path)
        } else {
            try ensureUpperDirLocked(RelPath.parent(path))
            try check(clonefileat(lower.rootFD, path, upper.rootFD, path, UInt32(CLONE_NOFOLLOW)), "clonefileat \(path)")
            dequarantineLocked(path)
            var u = try upper.stat(path)
            if u.flags & lockFlags != 0 {
                try upper.setCommon(path, birth: nil, backup: nil, flags: u.flags & ~lockFlags)
                u = try upper.stat(path)
            }
            alias[u.fileID] = l.fileID
            upperTypes[keys.key(path)] = l.vtype
        }
        copyUps += 1
        mutations &+= 1
        return true
    }

    private func existsLocked(_ path: String) -> Bool {
        upperTypeLocked(path) != nil || lowerEntryLocked(path) != nil
    }

    /// Adds a whiteout for `path`. Whiteouts below it are no longer needed.
    private func addWhiteoutLocked(_ path: String) throws {
        let k = keys.key(path)
        let prefix = k + "/"
        for key in removed.keys where key.hasPrefix(prefix) { removed[key] = nil }
        removed[k] = path
        try saveWhiteoutsLocked()
    }

    private func saveWhiteoutsLocked() throws {
        if removed.isEmpty {
            do { try upper.removeXattr(".", name: Overlay.removedAttr) } catch let e as PosixError where e.code == ENOATTR {}
            return
        }
        let data = try JSONSerialization.data(withJSONObject: removed.values.sorted(), options: [])
        try upper.setXattr(".", name: Overlay.removedAttr, value: data, options: 0)
    }

    /// Prepares a new upper entry at `path`: its parent folder exists in the upper folder.
    private func prepareNewLocked(_ path: String) throws {
        if !path.contains("/") && Overlay.reservedRootNames.contains(path) {
            throw PosixError(code: EPERM, op: "overlay reserved name \(path)")
        }
        if existsLocked(path) { throw PosixError(code: EEXIST, op: "overlay create \(path)") }
        let parent = RelPath.parent(path)
        if parent != "." && !existsLocked(parent) { throw PosixError(code: ENOENT, op: "overlay create \(path)") }
        try ensureUpperDirLocked(parent)
    }

    func createFile(_ path: String, mode: mode_t) throws {
        mu.lock()
        defer { mu.unlock() }
        try prepareNewLocked(path)
        try upper.createFile(path, mode: mode)
        dequarantineLocked(path)
        upperTypes[keys.key(path)] = .reg
        mutations &+= 1
    }

    func mkdir(_ path: String, mode: mode_t) throws {
        mu.lock()
        defer { mu.unlock() }
        try prepareNewLocked(path)
        try upper.mkdir(path, mode: mode)
        dequarantineLocked(path)
        upperTypes[keys.key(path)] = .dir
        mutations &+= 1
    }

    func symlink(_ path: String, target: String) throws {
        mu.lock()
        defer { mu.unlock() }
        try prepareNewLocked(path)
        try upper.symlink(path, target: target)
        dequarantineLocked(path)
        upperTypes[keys.key(path)] = .lnk
        mutations &+= 1
    }

    func link(from src: String, to dst: String) throws {
        mu.lock()
        defer { mu.unlock() }
        guard existsLocked(src) else { throw PosixError(code: ENOENT, op: "overlay link \(src)") }
        try prepareNewLocked(dst)
        _ = try copyUpLocked(src)
        try upper.link(from: src, to: dst)
        upperTypes[keys.key(dst)] = upperTypeLocked(src)
        mutations &+= 1
    }

    /// Removes the merged entry at `path`. A folder must be empty in the merged view.
    func unlink(_ path: String, isDir: Bool) throws {
        mu.lock()
        defer { mu.unlock() }
        try unlinkLocked(path, isDir: isDir)
    }

    private func unlinkLocked(_ path: String, isDir: Bool) throws {
        if path == "." { throw PosixError(code: EBUSY, op: "overlay remove root") }
        let ut = upperTypeLocked(path)
        let l = lowerEntryLocked(path)
        if ut == nil && l == nil { throw PosixError(code: ENOENT, op: "overlay remove \(path)") }
        let isDirNow = (ut ?? l!.vtype) == .dir
        if isDir && !isDirNow { throw PosixError(code: ENOTDIR, op: "overlay rmdir \(path)") }
        if !isDir && isDirNow { throw PosixError(code: EISDIR, op: "overlay unlink \(path)") }
        if try isDirNow && !listLocked(path).isEmpty { throw PosixError(code: ENOTEMPTY, op: "overlay rmdir \(path)") }
        if ut != nil {
            let u = try upper.stat(path)
            try upper.unlink(path, isDir: isDirNow)
            upperTypes[keys.key(path)] = nil
            if u.linkCount <= 1 || isDirNow { alias[u.fileID] = nil }
        }
        if l != nil { try addWhiteoutLocked(path) }
        mutations &+= 1
    }

    /// Renames within the merged view. A folder that the lower tree has fails with EXDEV: callers
    /// such as mv(1) then copy and remove.
    func rename(_ src: String, _ dst: String) throws {
        mu.lock()
        defer { mu.unlock() }
        if src == dst { return }
        let sut = upperTypeLocked(src)
        let sl = lowerEntryLocked(src)
        if sut == nil && sl == nil { throw PosixError(code: ENOENT, op: "overlay rename \(src)") }
        let type = sut ?? sl!.vtype
        if type == .dir && sl != nil { throw PosixError(code: EXDEV, op: "overlay rename of lower folder \(src)") }
        if type == .dir && (dst.hasPrefix(src + "/") || keys.key(dst).hasPrefix(keys.key(src) + "/")) {
            throw PosixError(code: EINVAL, op: "overlay rename \(src) into itself")
        }
        let dparent = RelPath.parent(dst)
        if dparent != "." && !existsLocked(dparent) { throw PosixError(code: ENOENT, op: "overlay rename to \(dst)") }
        // A rename that changes only the case of the name keeps the entry.
        let sameEntry = keys.key(src) == keys.key(dst)
        if !sameEntry && existsLocked(dst) {
            let dtype = upperTypeLocked(dst) ?? lowerEntryLocked(dst)!.vtype
            if type == .dir && dtype != .dir { throw PosixError(code: ENOTDIR, op: "overlay rename to \(dst)") }
            if type != .dir && dtype == .dir { throw PosixError(code: EISDIR, op: "overlay rename to \(dst)") }
            try unlinkLocked(dst, isDir: dtype == .dir)
        }
        try ensureUpperDirLocked(dparent)
        if sut != nil {
            try upper.rename(src, dst)
            let sk = keys.key(src), dk = keys.key(dst)
            upperTypes[sk] = nil
            upperTypes[dk] = type
            if type == .dir {
                let prefix = sk + "/"
                for (k, t) in upperTypes where k.hasPrefix(prefix) {
                    upperTypes[k] = nil
                    upperTypes[dk + "/" + k.dropFirst(prefix.count)] = t
                }
            }
        } else {
            try check(clonefileat(lower.rootFD, src, upper.rootFD, dst, UInt32(CLONE_NOFOLLOW)), "clonefileat \(src) -> \(dst)")
            dequarantineLocked(dst)
            var u = try upper.stat(dst)
            if u.flags & lockFlags != 0 {
                try upper.setCommon(dst, birth: nil, backup: nil, flags: u.flags & ~lockFlags)
                u = try upper.stat(dst)
            }
            alias[u.fileID] = sl!.fileID
            upperTypes[keys.key(dst)] = type
        }
        if sl != nil && !sameEntry { try addWhiteoutLocked(src) }
        mutations &+= 1
    }

    var json: [String: Any] {
        mu.lock()
        defer { mu.unlock() }
        return ["lower": lower.rootPath, "upper": upper.rootPath, "upper_entries": upperTypes.count,
                "whiteouts": removed.count, "copy_ups": copyUps, "quarantine_kept": quarantineKept, "case_sensitive": keys.caseSensitive,
                "lower_index": lowerIndex.countsJSON]
    }
}
