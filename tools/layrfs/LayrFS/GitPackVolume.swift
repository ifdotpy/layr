// LayrFS git pack volume (docs/git-on-objects.md): the git blobs of a project's object store as
// git packs. A folder with a layrfs-gitpack.conf file mounts in this mode. The file names the
// folder of the pipes of `layr fs gitpack serve`, which lists the packs and gives their bytes; the
// volume holds no data of its own. Layout: `/pack/pack-<id>.idx` and `/pack/pack-<id>.pack`, read
// only. The sandbox refuses Unix sockets, so a connection is a pair of named pipes: the volume
// makes `<id>.req` and `<id>.resp`, writes `<id>` to the pipe `ctl`, and the server answers "K".
//
// Requests (little endian):
//   'L'                                   -> u32 count, then per pack: u16 name length, name,
//                                            u64 index size, u64 pack size
//   'R' u8 kind (0 index, 1 pack) u16 n name u64 offset u32 length
//                                         -> u8 status (0 ok, else an errno), u32 n, n bytes

import Darwin
import Foundation
import FSKit

struct GitPackConfig {
    let pipes: String

    static let fileName = "layrfs-gitpack.conf"

    static func exists(resource path: String) -> Bool {
        FileManager.default.fileExists(atPath: (path as NSString).appendingPathComponent(fileName))
    }

    static func load(resource path: String) throws -> GitPackConfig {
        let text = try String(contentsOfFile: (path as NSString).appendingPathComponent(fileName), encoding: .utf8)
        for line in text.split(separator: "\n") {
            let parts = line.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            if parts.count == 2 && parts[0] == "pipes" { return GitPackConfig(pipes: parts[1]) }
        }
        throw PosixError(code: EINVAL, op: "layrfs-gitpack.conf names no pipes")
    }
}

/// Connections to the pack server, reused across requests.
final class PackClient {
    struct Conn { let r: Int32; let w: Int32 }
    private let dir: String
    private var idle: [Conn] = []
    private let lock = NSLock()

    init(dir: String) { self.dir = dir }

    deinit { for c in idle { Darwin.close(c.r); Darwin.close(c.w) } }

    private func path(_ name: String) -> String { (dir as NSString).appendingPathComponent(name) }

    /// A new pair of pipes: the response pipe opens first (it needs no writer), then the server
    /// learns the id, opens the request pipe for reading and the response pipe for writing, and
    /// says "K". Waits at most five seconds, so a stopped server fails a request, not the volume.
    private func connect() throws -> Conn {
        let id = UUID().uuidString
        let req = path(id + ".req"), resp = path(id + ".resp")
        for p in [req, resp] {
            unlink(p)
            if mkfifo(p, 0o600) != 0 { throw PosixError(code: errno, op: "mkfifo \(p)") }
        }
        var r: Int32 = -1, w: Int32 = -1
        do {
            r = open(resp, O_RDONLY | O_NONBLOCK)
            if r < 0 { throw PosixError(code: errno, op: "open \(resp)") }
            let ctl = open(path("ctl"), O_WRONLY | O_NONBLOCK)
            if ctl < 0 { throw PosixError(code: errno == ENXIO ? ECONNREFUSED : errno, op: "open the pipe of the pack server") }
            let line = Array((id + "\n").utf8)
            let n = line.withUnsafeBytes { Darwin.write(ctl, $0.baseAddress!, line.count) }
            Darwin.close(ctl)
            if n != line.count { throw PosixError(code: EIO, op: "write the pipe of the pack server") }
            var waited = 0
            while true {
                w = open(req, O_WRONLY | O_NONBLOCK)
                if w >= 0 { break }
                if errno != ENXIO || waited > 5000 { throw PosixError(code: errno == ENXIO ? ETIMEDOUT : errno, op: "open \(req)") }
                usleep(1000); waited += 1
            }
            var hello: UInt8 = 0
            var pfd = pollfd(fd: r, events: Int16(POLLIN), revents: 0)
            let deadline = 5000
            var spent = 0
            while true {
                let n = Darwin.read(r, &hello, 1)
                if n == 1 { break }
                if n < 0 && errno != EAGAIN && errno != EINTR { throw PosixError(code: errno, op: "read \(resp)") }
                if spent > deadline { throw PosixError(code: ETIMEDOUT, op: "the pack server does not answer") }
                _ = poll(&pfd, 1, 10); spent += 10
            }
            if hello != UInt8(ascii: "K") { throw PosixError(code: EIO, op: "the pack server said no") }
            _ = fcntl(r, F_SETFL, fcntl(r, F_GETFL) & ~O_NONBLOCK)
            _ = fcntl(w, F_SETFL, fcntl(w, F_GETFL) & ~O_NONBLOCK)
            _ = fcntl(w, F_SETNOSIGPIPE, 1)
            unlink(req); unlink(resp)
            return Conn(r: r, w: w)
        } catch {
            if r >= 0 { Darwin.close(r) }
            if w >= 0 { Darwin.close(w) }
            unlink(req); unlink(resp)
            throw error
        }
    }

    private static func writeAll(_ fd: Int32, _ data: [UInt8]) throws {
        var done = 0
        while done < data.count {
            let n = data.withUnsafeBytes { Darwin.write(fd, $0.baseAddress! + done, data.count - done) }
            if n < 0 { if errno == EINTR { continue }; throw PosixError(code: errno, op: "write") }
            done += n
        }
    }

    private static func readAll(_ fd: Int32, into p: UnsafeMutableRawPointer, _ count: Int) throws {
        var done = 0
        while done < count {
            let n = Darwin.read(fd, p + done, count - done)
            if n < 0 { if errno == EINTR { continue }; throw PosixError(code: errno, op: "read") }
            if n == 0 { throw PosixError(code: EIO, op: "the pack server closed the connection") }
            done += n
        }
    }

    private static func readBytes(_ fd: Int32, _ count: Int) throws -> [UInt8] {
        var b = [UInt8](repeating: 0, count: count)
        try b.withUnsafeMutableBytes { try readAll(fd, into: $0.baseAddress!, count) }
        return b
    }

    private static func u32(_ b: [UInt8], _ at: Int) -> UInt32 {
        UInt32(b[at]) | UInt32(b[at + 1]) << 8 | UInt32(b[at + 2]) << 16 | UInt32(b[at + 3]) << 24
    }

    private static func u64(_ b: [UInt8], _ at: Int) -> UInt64 {
        UInt64(u32(b, at)) | UInt64(u32(b, at + 4)) << 32
    }

    /// Runs `body` with a connection; a connection that failed is closed, not reused.
    private func with<R>(_ body: (Conn) throws -> R) throws -> R {
        lock.lock()
        let c = idle.popLast()
        lock.unlock()
        let conn = try c ?? connect()
        do {
            let r = try body(conn)
            lock.lock(); idle.append(conn); lock.unlock()
            return r
        } catch {
            Darwin.close(conn.r); Darwin.close(conn.w)
            throw error
        }
    }

    struct PackInfo {
        let name: String
        let indexSize: UInt64
        let packSize: UInt64
    }

    func list() throws -> [PackInfo] {
        try with { c in
            try Self.writeAll(c.w, [UInt8(ascii: "L")])
            let count = Int(Self.u32(try Self.readBytes(c.r, 4), 0))
            var out: [PackInfo] = []
            for _ in 0..<count {
                let h = try Self.readBytes(c.r, 2)
                let n = Int(h[0]) | Int(h[1]) << 8
                let name = String(decoding: try Self.readBytes(c.r, n), as: UTF8.self)
                let sizes = try Self.readBytes(c.r, 16)
                out.append(PackInfo(name: name, indexSize: Self.u64(sizes, 0), packSize: Self.u64(sizes, 8)))
            }
            return out
        }
    }

    /// Reads up to `length` bytes of the index (kind 0) or the pack (kind 1) into `into`.
    func read(kind: UInt8, name: String, offset: UInt64, length: Int, into p: UnsafeMutableRawBufferPointer) throws -> Int {
        try with { c in
            var req: [UInt8] = [UInt8(ascii: "R"), kind]
            let nb = Array(name.utf8)
            req += [UInt8(nb.count & 0xff), UInt8(nb.count >> 8)] + nb
            for i in 0..<8 { req.append(UInt8((offset >> (8 * UInt64(i))) & 0xff)) }
            let len = UInt32(min(length, p.count))
            for i in 0..<4 { req.append(UInt8((len >> (8 * UInt32(i))) & 0xff)) }
            try Self.writeAll(c.w, req)
            let h = try Self.readBytes(c.r, 5)
            let n = Int(Self.u32(h, 1))
            if h[0] != 0 {
                if n > 0 { _ = try Self.readBytes(c.r, n) }
                throw PosixError(code: Int32(h[0]), op: "pack read")
            }
            guard n <= p.count else { throw PosixError(code: EIO, op: "the pack server sent too much") }
            if n > 0 { try Self.readAll(c.r, into: p.baseAddress!, n) }
            return n
        }
    }
}

final class GitPackItem: FSItem {
    let ino: UInt64
    let type: FSItem.ItemType
    let name: String
    init(ino: UInt64, type: FSItem.ItemType, name: String) {
        self.ino = ino
        self.type = type
        self.name = name
        super.init()
    }
}

final class GitPackVolume: FSVolume, FSVolume.Handler, FSVolume.PathConfOperations,
    FSVolume.OpenCloseHandler, FSVolume.ReadWriteHandler
{
    static let rootIno: UInt64 = 2
    static let packDirIno: UInt64 = 3
    /// Pack `k` of the listing: its index at `first + 2k`, its pack at `first + 2k + 1`.
    static let first: UInt64 = 16

    let client: PackClient
    let root: GitPackItem
    let packDir: GitPackItem
    let started: timespec
    let uid: uid_t
    let gid: gid_t
    private let lock = NSLock()
    /// Every pack seen since the mount, by name: an open file of a pack that left the listing
    /// still reads. `current` is the listing.
    private var known: [String: (k: UInt64, info: PackClient.PackInfo)] = [:]
    private var byIno: [UInt64: GitPackItem] = [:]
    private var current: [String] = []
    private var listed = timespec()

    init(resource: FSPathURLResource, config: GitPackConfig, volumeID: UUID) throws {
        client = PackClient(dir: config.pipes)
        root = GitPackItem(ino: Self.rootIno, type: .directory, name: "")
        packDir = GitPackItem(ino: Self.packDirIno, type: .directory, name: "pack")
        var now = timespec()
        clock_gettime(CLOCK_REALTIME, &now)
        started = now
        var st = stat()
        if lstat(resource.url.path, &st) != 0 { throw PosixError(code: errno, op: "stat \(resource.url.path)") }
        uid = st.st_uid
        gid = st.st_gid
        super.init(volumeID: FSVolume.Identifier(uuid: volumeID), volumeName: FSFileName(string: "layr-gitpack"))
        _ = try refresh(force: true)
    }

    /// Lists the packs again when the last listing is more than a second old.
    private func refresh(force: Bool) throws -> [String] {
        var now = timespec()
        clock_gettime(CLOCK_MONOTONIC, &now)
        lock.lock()
        if !force && listed.tv_sec != 0 && now.tv_sec - listed.tv_sec < 1 {
            let c = current
            lock.unlock()
            return c
        }
        lock.unlock()
        let packs = try client.list()
        lock.lock()
        defer { lock.unlock() }
        current = []
        for p in packs {
            if known[p.name] == nil { known[p.name] = (UInt64(known.count), p) }
            current.append(p.name)
        }
        listed = now
        return current
    }

    private func fileItem(name: String) -> GitPackItem? {
        let base: String, isPack: Bool
        if name.hasSuffix(".idx") { base = String(name.dropLast(4)); isPack = false }
        else if name.hasSuffix(".pack") { base = String(name.dropLast(5)); isPack = true }
        else { return nil }
        lock.lock()
        defer { lock.unlock() }
        guard current.contains(base), let entry = known[base] else { return nil }
        let ino = Self.first + 2 * entry.k + (isPack ? 1 : 0)
        if let it = byIno[ino] { return it }
        let it = GitPackItem(ino: ino, type: .file, name: name)
        byIno[ino] = it
        return it
    }

    /// The pack name, kind (0 index, 1 pack) and size of a file item.
    private func file(_ it: GitPackItem) throws -> (String, UInt8, UInt64) {
        let k = (it.ino - Self.first) / 2
        let isPack = (it.ino - Self.first) % 2 == 1
        lock.lock()
        defer { lock.unlock() }
        guard let entry = known.first(where: { $0.value.k == k }) else { throw PosixError(code: ENOENT, op: "pack") }
        return (entry.key, isPack ? 1 : 0, isPack ? entry.value.info.packSize : entry.value.info.indexSize)
    }

    private func attributes(_ it: GitPackItem) throws -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.type = it.type
        a.uid = uid
        a.gid = gid
        a.flags = 0
        a.fileID = it.ino == Self.rootIno ? .rootDirectory : (FSItem.Identifier(rawValue: it.ino) ?? .invalid)
        a.parentID = it.ino == Self.rootIno ? .parentOfRoot : (it.ino == Self.packDirIno ? .rootDirectory : (FSItem.Identifier(rawValue: Self.packDirIno) ?? .invalid))
        if it.type == .directory {
            a.mode = UInt32(S_IFDIR) | 0o555
            a.linkCount = 2
            a.size = 0
            a.allocSize = 0
        } else {
            // Writable only for the time change of git's "freshen" (setAttributes); writes fail.
            a.mode = UInt32(S_IFREG) | 0o644
            a.linkCount = 1
            let size = try file(it).2
            a.size = size
            a.allocSize = (size + 4095) / 4096 * 4096
        }
        a.supportsLimitedXAttrs = false
        a.inhibitKernelOffloadedIO = false
        a.modifyTime = started
        a.changeTime = started
        a.accessTime = started
        a.birthTime = started
        a.backupTime = timespec()
        a.addedTime = started
        return a
    }

    private func item(_ i: FSItem) throws -> GitPackItem {
        guard let it = i as? GitPackItem else { throw PosixError(code: EINVAL, op: "foreign FSItem") }
        return it
    }

    private func run<R>(_ reply: @escaping @Sendable (R?, (any Error)?) -> Void, _ body: () throws -> R) {
        do { reply(try body(), nil) } catch let e as PosixError { reply(nil, fs_errorForPOSIXError(e.code)) } catch { reply(nil, error) }
    }

    private func runVoid(_ reply: @escaping @Sendable ((any Error)?) -> Void, _ body: () throws -> Void) {
        do { try body(); reply(nil) } catch let e as PosixError { reply(fs_errorForPOSIXError(e.code)) } catch { reply(error) }
    }

    // MARK: - PathConfOperations

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumFileSizeInBits: Int { 64 }

    // MARK: - Volume

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsPersistentObjectIDs = false
        c.supportsFastStatFS = true
        c.supports2TBFiles = true
        c.supports64BitObjectIDs = true
        c.caseFormat = .sensitive
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let r = FSStatFSResult(fileSystemTypeName: "layrfs")
        r.blockSize = 4096
        r.ioSize = 1 << 20
        return r
    }

    var enableOpenUnlinkEmulation: Bool { false }

    func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void) {
        run(reply) {
            guard let r = FSActivateResult(rootItem: root) else { throw PosixError(code: EIO, op: "FSActivateResult") }
            return r
        }
    }

    func deactivateVolume(options: FSDeactivateOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) { reply(nil) }
    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) { reply(nil) }
    func unmount(replyHandler reply: @escaping @Sendable () -> Void) { reply() }
    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) { reply(nil) }

    // MARK: - Items

    func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)
    {
        run(reply) {
            let dir = try item(directory)
            guard let n = name.string else { throw PosixError(code: ENOENT, op: "lookup") }
            let found: GitPackItem
            switch (dir.ino, n) {
            case (_, "."): found = dir
            case (Self.packDirIno, ".."), (Self.rootIno, ".."): found = root
            case (Self.rootIno, "pack"): found = packDir
            case (Self.packDirIno, _):
                _ = try refresh(force: false)
                guard let f = fileItem(name: n) else { throw PosixError(code: ENOENT, op: "lookup") }
                found = f
            default: throw PosixError(code: ENOENT, op: "lookup")
            }
            guard let r = FSLookupItemResult(foundItem: found, itemName: name, itemAttributes: try attributes(found)) else {
                throw PosixError(code: EIO, op: "FSLookupItemResult")
            }
            return r
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        if let it = item as? GitPackItem, it.ino >= Self.first {
            lock.lock(); byIno[it.ino] = nil; lock.unlock()
        }
        reply(nil)
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext,
                       replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)
    {
        run(reply) {
            guard let r = FSGetAttributesResult(attributes: try attributes(try self.item(item))) else {
                throw PosixError(code: EIO, op: "FSGetAttributesResult")
            }
            return r
        }
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext,
                       replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)
    {
        run(reply) {
            let it = try self.item(item)
            // git touches a pack before it skips the write of an object that the pack holds; a
            // refused touch makes it write a loose copy. Times stay as they are.
            for a: FSItem.Attribute in [.mode, .uid, .gid, .size, .flags] where newAttributes.isValid(a) {
                throw PosixError(code: EPERM, op: "setattr")
            }
            var used: FSItem.Attribute = []
            for a: FSItem.Attribute in [.accessTime, .modifyTime, .birthTime, .backupTime] where newAttributes.isValid(a) { used.insert(a) }
            newAttributes.consumedAttributes = used
            guard let r = FSSetAttributesResult(attributes: try attributes(it), freeSpace: FSFreeSpace.noUpdate) else {
                throw PosixError(code: EIO, op: "FSSetAttributesResult")
            }
            return r
        }
    }

    func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem,
                    attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }

    func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName, context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }

    func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }

    func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
                    to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?,
                    context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }

    func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }

    func readSymbolicLink(_ item: FSItem, context: FSContext,
                          replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EINVAL))
    }

    func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
                            attributes attrReq: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
                            context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)
    {
        run(reply) {
            let dir = try item(directory)
            var entries: [GitPackItem] = []
            if dir.ino == Self.rootIno {
                entries = [packDir]
            } else if dir.ino == Self.packDirIno {
                for name in try refresh(force: cookie == .initial) {
                    for f in [name + ".idx", name + ".pack"] {
                        if let it = fileItem(name: f) { entries.append(it) }
                    }
                }
            } else {
                throw PosixError(code: ENOTDIR, op: "enumerate")
            }
            let withAttrs = attrReq != nil
            var all: [(String, GitPackItem)] = withAttrs ? [] : [(".", dir), ("..", root)]
            all += entries.map { ($0.name, $0) }
            let start = Int(cookie.rawValue)
            if start > all.count { throw FSError(.invalidDirectoryCookie) }
            var i = start
            while i < all.count {
                let (n, it) = all[i]
                let id: FSItem.Identifier = it.ino == Self.rootIno ? .rootDirectory : (FSItem.Identifier(rawValue: it.ino) ?? .invalid)
                let ok = packer.packEntry(name: FSFileName(string: n), itemType: it.type, itemID: id,
                                          nextCookie: FSDirectoryCookie(UInt64(i + 1)),
                                          attributes: withAttrs ? try attributes(it) : nil)
                if !ok { break }
                i += 1
            }
            guard let r = FSEnumerateDirectoryResult(verifier: UInt64(current.count + 1)) else {
                throw PosixError(code: EIO, op: "FSEnumerateDirectoryResult")
            }
            return r
        }
    }

    // MARK: - Open, close, read

    var isOpenCloseInhibited: Bool { false }

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                  replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        reply(modes.contains(.write) ? fs_errorForPOSIXError(EROFS) : nil)
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                   replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        reply(nil)
    }

    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
              replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)
    {
        run(reply) {
            let it = try self.item(item)
            guard it.type == .file else { throw PosixError(code: EISDIR, op: "read") }
            let (name, kind, size) = try file(it)
            var got = 0
            if UInt64(offset) < size {
                let want = Int(min(UInt64(length), size - UInt64(offset)))
                got = try buffer.withUnsafeMutableBytes { (p: UnsafeMutableRawBufferPointer) -> Int in
                    var done = 0
                    let total = min(want, p.count)
                    // The server answers at most a few MiB per request.
                    while done < total {
                        let part = UnsafeMutableRawBufferPointer(rebasing: p[done..<min(total, done + (4 << 20))])
                        let n = try client.read(kind: kind, name: name, offset: UInt64(offset) + UInt64(done), length: part.count, into: part)
                        if n == 0 { break }
                        done += n
                    }
                    return done
                }
            }
            guard let r = FSReadFileResult(bytesRead: got, itemAttributes: try attributes(it)) else {
                throw PosixError(code: EIO, op: "FSReadFileResult")
            }
            return r
        }
    }

    func write(contents: Data, to item: FSItem, at offset: off_t,
               replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)
    {
        reply(nil, fs_errorForPOSIXError(EROFS))
    }
}
