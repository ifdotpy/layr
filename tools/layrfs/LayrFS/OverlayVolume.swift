// LayrFS overlay volume: the merged view of a virtual line (Overlay.swift) with the FSKit V3 handlers.
// A resource folder with a `layrfs-view.conf` file mounts in this mode (LayrFileSystem.loadResource).

import Darwin
import Foundation
import FSKit

/// The settings of one overlay mount, from `<resource>/layrfs-view.conf` (key=value lines).
struct ViewConfig {
    static let fileName = "layrfs-view.conf"
    var lower = ""
    var upper = ""
    /// 1: read every lower folder in the background after the mount (else on first use).
    var prefetch = false
    var pairs: [(String, String)] = []

    static func load(resource: String) throws -> ViewConfig {
        let path = (resource as NSString).appendingPathComponent(fileName)
        let text = try String(contentsOfFile: path, encoding: .utf8)
        var c = ViewConfig()
        for line in text.split(whereSeparator: \.isNewline) {
            let l = line.trimmingCharacters(in: .whitespaces)
            if l.isEmpty || l.hasPrefix("#") { continue }
            let kv = l.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            guard kv.count == 2 else { continue }
            switch kv[0] {
            case "lower": c.lower = kv[1]
            case "upper": c.upper = kv[1]
            case "prefetch": c.prefetch = (kv[1] == "1" || kv[1] == "true")
            default: c.pairs.append((kv[0], kv[1]))
            }
        }
        if c.upper.isEmpty { c.upper = (resource as NSString).appendingPathComponent("upper") }
        if c.lower.isEmpty { throw PosixError(code: EINVAL, op: "\(fileName): no lower=") }
        return c
    }

    static func exists(resource: String) -> Bool {
        FileManager.default.fileExists(atPath: (resource as NSString).appendingPathComponent(fileName))
    }
}

final class OverlayVolume: FSVolume, FSVolume.Handler, FSVolume.PathConfOperations,
    FSVolume.OpenCloseHandler, FSVolume.ReadWriteHandler, FSVolume.XattrHandler,
    FSVolume.DataCacheHandler
{
    let resource: FSPathURLResource
    let ov: Overlay
    let table: ItemTable
    let config: Config
    let readOnly: Bool

    init(resource: FSPathURLResource, view: ViewConfig, config: Config, readOnly: Bool, volumeID: UUID) throws {
        self.resource = resource
        self.config = config
        self.readOnly = readOnly
        ov = try Overlay(lowerPath: view.lower, upperPath: view.upper)
        table = ItemTable(root: LayrItem(ino: ov.rootIno, type: .directory, parent: nil, name: ""))
        LayrItem.fdBudget.configure()
        umask(0)  // create modes come from the caller; the kernel already applied its umask
        let url = resource.url
        super.init(volumeID: FSVolume.Identifier(uuid: volumeID),
                   volumeName: FSFileName(string: url.lastPathComponent.isEmpty ? "layrfs" : url.lastPathComponent))
        Stats.shared.setNote("mode", "overlay")
        Stats.shared.setNote("backing_path", url.path)
        Stats.shared.setNote("read_only", readOnly)
        Stats.shared.setNote("config", config.asJSON)
        Stats.shared.setNote("overlay", ov.json)
        if view.prefetch {
            let ov = self.ov
            DispatchQueue.global(qos: .utility).async {
                ov.lowerIndex.loadAll()
                Stats.shared.setNote("overlay_prefetch", ov.lowerIndex.countsJSON)
            }
        }
        fsLog.notice("layrfs overlay init lower=\(view.lower, privacy: .public) upper=\(view.upper, privacy: .public) rootIno=\(self.ov.rootIno)")
    }

    // MARK: - Helpers

    private func err(_ e: Error) -> Error {
        if let p = e as? PosixError { return fs_errorForPOSIXError(p.code) }
        return e
    }

    private func code(_ e: Error) -> Int32 {
        if let p = e as? PosixError { return p.code }
        let ns = e as NSError
        return ns.domain == NSPOSIXErrorDomain ? Int32(ns.code) : EIO
    }

    private func run<R>(_ op: Op, bytes: ((R) -> Int)? = nil,
                        _ reply: @escaping @Sendable (R?, (any Error)?) -> Void, _ body: () throws -> R)
    {
        let t0 = nowNs()
        do {
            let r = try body()
            Stats.shared.record(op, ns: nowNs() - t0, bytes: bytes?(r) ?? 0)
            reply(r, nil)
        } catch {
            Stats.shared.record(op, ns: nowNs() - t0, error: code(error))
            reply(nil, err(error))
        }
    }

    private func runVoid(_ op: Op, _ reply: @escaping @Sendable ((any Error)?) -> Void, _ body: () throws -> Void) {
        let t0 = nowNs()
        do {
            try body()
            Stats.shared.record(op, ns: nowNs() - t0)
            reply(nil)
        } catch {
            Stats.shared.record(op, ns: nowNs() - t0, error: code(error))
            reply(err(error))
        }
    }

    private func requireWritable() throws {
        if readOnly { throw PosixError(code: EROFS, op: "read-only volume") }
    }

    private func id(_ ino: UInt64) -> FSItem.Identifier {
        if ino == ov.rootIno { return .rootDirectory }
        return FSItem.Identifier(rawValue: ino) ?? .invalid
    }

    private func itemType(_ e: Entry) -> FSItem.ItemType {
        switch e.vtype {
        case .reg: return .file
        case .dir: return .directory
        case .lnk: return .symlink
        case .fifo: return .fifo
        case .chr: return .charDevice
        case .blk: return .blockDevice
        case .sock: return .socket
        default: return .unknown
        }
    }

    /// All attributes; the parent ID comes from the item table (the backing parents differ by layer).
    private func attributes(_ e: Entry, parent: UInt64?) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.type = itemType(e)
        a.mode = e.fullMode
        a.uid = e.uid
        a.gid = e.gid
        a.linkCount = e.linkCount
        a.flags = e.flags & ~lockFlags
        a.size = e.size
        a.allocSize = e.allocSize
        a.fileID = id(e.fileID)
        a.parentID = e.fileID == ov.rootIno ? .parentOfRoot : id(parent ?? ov.rootIno)
        a.supportsLimitedXAttrs = false
        a.inhibitKernelOffloadedIO = false
        a.modifyTime = e.modifyTime
        a.changeTime = e.changeTime
        a.accessTime = e.accessTime
        a.birthTime = e.birthTime
        a.backupTime = e.backupTime
        a.addedTime = e.addedTime
        return a
    }

    private func parentIno(_ it: LayrItem) -> UInt64 {
        table.lock.lock(); defer { table.lock.unlock() }
        return it.parent?.ino ?? ov.rootIno
    }

    private func attrs(_ e: Entry, of it: LayrItem) -> FSItem.Attributes { attributes(e, parent: parentIno(it)) }

    private func item(_ i: FSItem) throws -> LayrItem {
        guard let it = i as? LayrItem else { throw PosixError(code: EINVAL, op: "foreign FSItem") }
        return it
    }

    private func nameString(_ n: FSFileName) throws -> String {
        guard let s = n.string, !s.isEmpty, !s.contains("/") else { throw PosixError(code: EILSEQ, op: "file name") }
        return s
    }

    /// The merged attributes of an item; an open upper file answers from its descriptor.
    private func entryFor(_ it: LayrItem) throws -> Entry {
        if it.unlinked {
            if let e = try it.withFD(write: false, { try ov.fstat($0, ino: it.ino) }) { return e }
            throw PosixError(code: ESTALE, op: "removed item")
        }
        return try ov.stat(table.path(it))
    }

    /// Copies the item's file up before a change, and drops a descriptor of the lower copy.
    private func prepareChange(_ it: LayrItem, path: String) throws {
        if try ov.copyUp(path) { it.dropReadFD() }
    }

    // MARK: - FSVolume.PathConfOperations

    var maximumLinkCount: Int { 32767 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumFileSizeInBits: Int { 64 }

    // MARK: - FSVolume.Handler: volume

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsPersistentObjectIDs = true
        c.supportsSymbolicLinks = true
        c.supportsHardLinks = true
        c.supportsSparseFiles = true
        c.supportsFastStatFS = true
        c.supports2TBFiles = true
        c.supports64BitObjectIDs = true
        c.supportsHiddenFiles = true
        c.caseFormat = ov.keys.caseSensitive ? .sensitive : .insensitiveCasePreserving
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let t0 = nowNs()
        let r = FSStatFSResult(fileSystemTypeName: "layrfs")
        if let s = try? ov.statfs() {
            let bs = UInt64(s.f_bsize)
            r.blockSize = Int(s.f_bsize)
            r.ioSize = Int(s.f_iosize)
            r.totalBlocks = s.f_blocks
            r.availableBlocks = s.f_bavail
            r.freeBlocks = s.f_bfree
            r.usedBlocks = s.f_blocks - s.f_bfree
            r.totalBytes = s.f_blocks * bs
            r.availableBytes = s.f_bavail * bs
            r.freeBytes = s.f_bfree * bs
            r.usedBytes = (s.f_blocks - s.f_bfree) * bs
            r.totalFiles = s.f_files
            r.freeFiles = s.f_ffree
        }
        Stats.shared.record(.statfs, ns: nowNs() - t0)
        return r
    }

    var enableOpenUnlinkEmulation: Bool { false }

    func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void) {
        run(.activate, reply) {
            guard let r = FSActivateResult(rootItem: table.root) else { throw PosixError(code: EIO, op: "FSActivateResult") }
            return r
        }
    }

    func deactivateVolume(options: FSDeactivateOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        runVoid(.deactivate, reply) {
            Stats.shared.setNote("items_in_table_at_deactivate", table.count)
            Stats.shared.setNote("overlay", ov.json)
            Stats.shared.dump(reason: "deactivate")
        }
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        runVoid(.mount, reply) {}
    }

    func unmount(replyHandler reply: @escaping @Sendable () -> Void) {
        Stats.shared.record(.unmount, ns: 0)
        Stats.shared.setNote("overlay", ov.json)
        Stats.shared.dump(reason: "unmount")
        reply()
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        runVoid(.synchronize, reply) {}  // every write is already in the upper folder
    }

    // MARK: - FSVolume.Handler: items

    func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)
    {
        run(.lookup, reply) {
            let dir = try item(directory)
            let n = try nameString(name)
            let found: LayrItem
            let e: Entry
            if n == "." || n == ".." {
                table.lock.lock()
                found = (n == "." ? dir : (dir.parent ?? table.root))
                table.lock.unlock()
                e = try entryFor(found)
            } else {
                e = try ov.stat(table.childPath(dir, n))
                let diskName = e.name.isEmpty ? n : e.nameString
                found = table.resolve(e, type: itemType(e), parent: dir, name: diskName)
            }
            let outName = e.name.isEmpty ? name : FSFileName(data: Data(e.name))
            guard let r = FSLookupItemResult(foundItem: found, itemName: outName, itemAttributes: attrs(e, of: found)) else {
                throw PosixError(code: EIO, op: "FSLookupItemResult")
            }
            return r
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        let t0 = nowNs()
        guard let it = item as? LayrItem else { reply(nil); return }
        table.lock.lock()
        let ran = it.tryReclaim { self.table.reclaimLocked(it) }
        table.lock.unlock()
        if ran { it.closeFD() }
        Stats.shared.record(.reclaim, ns: nowNs() - t0)
        reply(nil)
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext,
                       replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)
    {
        run(.getattr, reply) {
            let it = try self.item(item)
            guard let r = FSGetAttributesResult(attributes: attrs(try entryFor(it), of: it)) else {
                throw PosixError(code: EIO, op: "FSGetAttributesResult")
            }
            return r
        }
    }

    /// Applies a set-attributes request to the upper copy at `path`. Returns the consumed attributes.
    private func apply(_ req: FSItem.SetAttributesRequest, to path: String, item it: LayrItem?,
                       skipMode: Bool = false) throws -> FSItem.Attribute
    {
        let up = ov.upper
        var used: FSItem.Attribute = []
        if !skipMode && req.isValid(.mode) {
            try up.chmod(path, mode: mode_t(req.mode & 0o7777)); used.insert(.mode)
        }
        let wantUID = req.isValid(.uid), wantGID = req.isValid(.gid)
        if wantUID || wantGID {
            try up.chown(path, uid: wantUID ? req.uid : uid_t.max, gid: wantGID ? req.gid : gid_t.max)
            if wantUID { used.insert(.uid) }
            if wantGID { used.insert(.gid) }
        }
        if req.isValid(.size) {
            if it?.type == .file || it == nil {
                if let it, let done = try it.withFD(write: true, { fd -> Bool in
                    try up.truncate(path, fd: fd, size: req.size); return true
                }), done {
                } else {
                    try up.truncate(path, fd: nil, size: req.size)
                }
            }
            used.insert(.size)
        }
        let wantA = req.isValid(.accessTime), wantM = req.isValid(.modifyTime)
        if wantA || wantM {
            try up.utimes(path, atime: wantA ? req.accessTime : nil, mtime: wantM ? req.modifyTime : nil)
            if wantA { used.insert(.accessTime) }
            if wantM { used.insert(.modifyTime) }
        }
        let wantB = req.isValid(.birthTime), wantK = req.isValid(.backupTime), wantF = req.isValid(.flags)
        if wantB || wantK || wantF {
            try up.setCommon(path, birth: wantB ? req.birthTime : nil, backup: wantK ? req.backupTime : nil,
                             flags: wantF ? req.flags & ~lockFlags : nil)
            if wantB { used.insert(.birthTime) }
            if wantK { used.insert(.backupTime) }
            if wantF { used.insert(.flags) }
        }
        req.consumedAttributes = used
        return used
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext,
                       replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)
    {
        run(.setattr, reply) {
            try requireWritable()
            let it = try self.item(item)
            let p = try table.path(it)
            try prepareChange(it, path: p)
            _ = try apply(newAttributes, to: p, item: it)
            guard let r = FSSetAttributesResult(attributes: attrs(try ov.stat(p), of: it), freeSpace: FSFreeSpace.noUpdate) else {
                throw PosixError(code: EIO, op: "FSSetAttributesResult")
            }
            return r
        }
    }

    private func dropSnapshot(_ dirs: LayrItem...) {
        table.lock.lock()
        for d in dirs { d.snapshot = nil }
        table.lock.unlock()
    }

    func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem,
                    attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)
    {
        run(type == .directory ? .mkdir : .create, reply) {
            try requireWritable()
            let dir = try item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            let mode = mode_t(newAttributes.isValid(.mode) ? newAttributes.mode & 0o7777 : (type == .directory ? 0o755 : 0o644))
            switch type {
            case .file: try ov.createFile(p, mode: mode)
            case .directory: try ov.mkdir(p, mode: mode)
            default: throw PosixError(code: ENOTSUP, op: "create type \(type.rawValue)")
            }
            dropSnapshot(dir)
            let rest = try apply(newAttributes, to: p, item: nil, skipMode: true)
            newAttributes.consumedAttributes = rest.union(newAttributes.isValid(.mode) ? .mode : [])
            let e = try ov.stat(p)
            let d = try entryFor(dir)
            let it = table.resolve(e, type: itemType(e), parent: dir, name: n)
            guard let r = FSCreateItemResult(newItem: it, newItemName: name, newItemAttributes: attributes(e, parent: dir.ino),
                                             directoryAttributes: attrs(d, of: dir), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateItemResult") }
            return r
        }
    }

    func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName, context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)
    {
        run(.symlink, reply) {
            try requireWritable()
            let dir = try item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            guard let target = contents.string else { throw PosixError(code: EILSEQ, op: "symlink target") }
            try ov.symlink(p, target: target)
            dropSnapshot(dir)
            _ = try? apply(newAttributes, to: p, item: nil)
            let e = try ov.stat(p)
            let d = try entryFor(dir)
            let it = table.resolve(e, type: .symlink, parent: dir, name: n)
            guard let r = FSCreateSymlinkResult(newItem: it, newItemName: name, newItemAttributes: attributes(e, parent: dir.ino),
                                                directoryAttributes: attrs(d, of: dir), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateSymlinkResult") }
            return r
        }
    }

    func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)
    {
        run(.link, reply) {
            try requireWritable()
            let it = try self.item(item)
            let dir = try self.item(directory)
            let n = try nameString(name)
            let dst = try table.childPath(dir, n)
            let src = try table.path(it)
            let copied = !ov.inUpper(src)
            try ov.link(from: src, to: dst)
            if copied { it.dropReadFD() }
            dropSnapshot(dir)
            let e = try ov.stat(dst)
            let d = try entryFor(dir)
            guard let r = FSCreateLinkResult(linkName: name, linkAttributes: attributes(e, parent: dir.ino),
                                             directoryAttributes: attrs(d, of: dir), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateLinkResult") }
            return r
        }
    }

    func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
                    to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?,
                    context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)
    {
        run(.rename, reply) {
            try requireWritable()
            let it = try self.item(item)
            let src = try self.item(sourceDirectory)
            let dst = try self.item(destinationDirectory)
            let over = try overItem.map { try self.item($0) }
            let sn = try nameString(sourceName)
            let dn = try nameString(destinationName)
            let sp = try table.childPath(src, sn)
            let dp = try table.childPath(dst, dn)
            var overEntry: Entry? = nil
            if over != nil { overEntry = try? ov.stat(dp) }
            let copied = !ov.inUpper(sp)
            try ov.rename(sp, dp)
            if copied { it.dropReadFD() }
            table.lock.lock()
            table.moveLocked(it, to: dst, name: dn)
            it.stalePath = false
            src.snapshot = nil
            dst.snapshot = nil
            if let over, over !== it, var oe = overEntry {
                oe.linkCount = oe.linkCount > 0 ? oe.linkCount - 1 : 0
                overEntry = oe
                if oe.linkCount == 0 || over.type == .directory { over.unlinked = true; table.detachLocked(over) }
                else { over.stalePath = true }
            }
            table.lock.unlock()
            let e = try ov.stat(dp)
            let sa = try entryFor(src)
            let da = src === dst ? sa : try entryFor(dst)
            guard let r = FSRenameItemResult(newName: destinationName, renamedItemAttributes: attributes(e, parent: dst.ino),
                                             sourceDirectoryAttributes: attrs(sa, of: src),
                                             destinationDirectoryAttributes: attrs(da, of: dst),
                                             overItemAttributes: overEntry.map { attributes($0, parent: dst.ino) },
                                             freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSRenameItemResult") }
            return r
        }
    }

    func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)
    {
        run(.remove, reply) {
            try requireWritable()
            let it = try self.item(item)
            let dir = try self.item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            var pre = try ov.stat(p)
            try ov.unlink(p, isDir: it.type == .directory)
            pre.linkCount = (it.type == .directory || pre.linkCount <= 1) ? 0 : pre.linkCount - 1
            table.lock.lock()
            dir.snapshot = nil
            if pre.linkCount == 0 { it.unlinked = true; table.detachLocked(it) } else { it.stalePath = true }
            table.lock.unlock()
            let d = try entryFor(dir)
            guard let r = FSRemoveItemResult(itemAttributes: attributes(pre, parent: dir.ino), directoryAttributes: attrs(d, of: dir),
                                             freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSRemoveItemResult") }
            return r
        }
    }

    func readSymbolicLink(_ item: FSItem, context: FSContext,
                          replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)
    {
        run(.readlink, reply) {
            let it = try self.item(item)
            let p = try table.path(it)
            let target = try ov.readlink(p)
            let e = try ov.stat(p)
            guard let r = FSReadSymlinkResult(contents: FSFileName(data: Data(target)), symlinkAttributes: attrs(e, of: it))
            else { throw PosixError(code: EIO, op: "FSReadSymlinkResult") }
            return r
        }
    }

    /// Cookie c means "start at index c" of the merged listing, kept while the enumeration runs.
    /// The verifier is the overlay generation: every change of the view increments it.
    func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
                            attributes attrReq: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
                            context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)
    {
        run(.enumerate, reply) {
            let dir = try item(directory)
            let withAttrs = attrReq != nil
            table.lock.lock()
            var snap = dir.snapshot
            let parent = dir.parent ?? table.root
            table.lock.unlock()
            let gen = ov.generation
            if cookie == .initial || snap == nil || snap!.verifier != verifier.rawValue || snap!.withAttributes != withAttrs {
                var entries = try ov.list(table.path(dir))
                if !withAttrs {
                    var dot = Entry(); dot.name = Array(".".utf8); dot.objType = VType.dir.rawValue; dot.fileID = dir.ino
                    var dotdot = Entry(); dotdot.name = Array("..".utf8); dotdot.objType = VType.dir.rawValue; dotdot.fileID = parent.ino
                    entries.insert(contentsOf: [dot, dotdot], at: 0)
                }
                snap = DirSnapshot(verifier: gen, withAttributes: withAttrs, entries: entries)
            }
            let s = snap!
            let start = Int(cookie.rawValue)
            if start > s.entries.count { throw FSError(.invalidDirectoryCookie) }
            var i = start
            while i < s.entries.count {
                let e = s.entries[i]
                let ok = packer.packEntry(name: FSFileName(data: Data(e.name)), itemType: itemType(e), itemID: id(e.fileID),
                                          nextCookie: FSDirectoryCookie(UInt64(i + 1)),
                                          attributes: withAttrs ? attributes(e, parent: dir.ino) : nil)
                if !ok { break }
                i += 1
            }
            table.lock.lock()
            dir.snapshot = i < s.entries.count ? s : nil
            table.lock.unlock()
            guard let r = FSEnumerateDirectoryResult(verifier: s.verifier) else {
                throw PosixError(code: EIO, op: "FSEnumerateDirectoryResult")
            }
            return r
        }
    }

    // MARK: - FSVolume.OpenCloseHandler (used when datacache=off)

    var isOpenCloseInhibited: Bool { !config.openClose }

    private func doOpen(_ it: LayrItem, modes: FSVolume.OpenModes) throws {
        if it.type != .file { return }
        let p = try table.path(it)
        if modes.contains(.write) {
            try requireWritable()
            try prepareChange(it, path: p)
        }
        try it.ensureOpen(path: p, backend: ov, write: modes.contains(.write))
    }

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                  replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        guard let it = item as? LayrItem else { reply(fs_errorForPOSIXError(EINVAL)); return }
        runVoid(.open, reply) { try doOpen(it, modes: modes) }
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                   replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        runVoid(.close, reply) {
            let it = try self.item(item)
            if modes.isEmpty { it.closeFD() }
        }
    }

    // MARK: - FSVolume.DataCacheHandler

    var isDataCacheInhibited: Bool { config.dataCache == .off }

    private func grant(_ mode: FSVolume.DataCacheMode) -> FSVolume.KernelCacheCoherencyType {
        switch (config.dataCache, mode) {
        case (_, .none), (.nocache, _), (.off, _): return .noCache
        case (_, .readWithCache): return .readCache
        case (.read, .readWriteWithCache): return .readCache
        case (.writethrough, .readWriteWithCache): return .writeThrough
        case (.writeback, .readWriteWithCache): return .writeBack
        default: return .noCache
        }
    }

    func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext,
              replyHandler reply: @escaping @Sendable (FSOpenItemResult?, (any Error)?) -> Void)
    {
        let g = grant(cacheMode)
        guard let it = item as? LayrItem else { reply(nil, fs_errorForPOSIXError(EINVAL)); return }
        run(.open, reply) {
            try doOpen(it, modes: modes)
            return FSOpenItemResult(grantedCoherency: g)
        }
    }

    func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void) {
        let t0 = nowNs()
        if let it = item as? LayrItem { it.closeFD() }
        Stats.shared.record(.close, ns: nowNs() - t0)
        reply()
    }

    func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext,
                 replyHandler reply: @escaping @Sendable (FSUpgradeItemResult?, (any Error)?) -> Void)
    {
        let g = grant(cacheMode)
        run(.cacheUpgrade, reply) {
            let it = try self.item(item)
            if cacheMode == .readWriteWithCache && it.type == .file && !readOnly {
                let p = try table.path(it)
                try prepareChange(it, path: p)
                try it.ensureOpen(path: p, backend: ov, write: true)
            }
            return FSUpgradeItemResult(grantedCoherency: g)
        }
    }

    // MARK: - FSVolume.ReadWriteHandler

    /// Runs `body` with the item's open descriptor, a cached read descriptor, or a temporary one.
    private func withIO<R>(_ it: LayrItem, write: Bool, _ body: (Int32) throws -> R) throws -> R {
        if let r = try it.withFD(write: write, body) { return r }
        if !write, try it.openCached(path: table.path(it), backend: ov), let r = try it.withFD(write: false, body) { return r }
        Stats.shared.countTempOpen()
        let fd = try ov.openFile(table.path(it), flags: write ? O_WRONLY : O_RDONLY, mode: 0)
        defer { Darwin.close(fd) }
        return try body(fd)
    }

    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
              replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)
    {
        run(.read, reply) {
            let it = try self.item(item)
            var got = 0
            try withIO(it, write: false) { fd in
                got = try buffer.withUnsafeMutableBytes { (p: UnsafeMutableRawBufferPointer) -> Int in
                    let want = min(length, p.count)
                    var done = 0
                    while done < want {
                        let n = pread(fd, p.baseAddress! + done, want - done, offset + off_t(done))
                        if n < 0 { if errno == EINTR { continue }; throw PosixError(code: errno, op: "pread") }
                        if n == 0 { break }
                        done += n
                    }
                    return done
                }
            }
            let e = try entryFor(it)
            Stats.shared.addBytes(.read, got)
            guard let r = FSReadFileResult(bytesRead: got, itemAttributes: attrs(e, of: it)) else {
                throw PosixError(code: EIO, op: "FSReadFileResult")
            }
            return r
        }
    }

    func write(contents: Data, to item: FSItem, at offset: off_t,
               replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)
    {
        run(.write, bytes: { _ in contents.count }, reply) {
            try requireWritable()
            let it = try self.item(item)
            if !it.unlinked { try prepareChange(it, path: table.path(it)) }
            var put = 0
            let e: Entry = try withIO(it, write: true) { fd in
                try contents.withUnsafeBytes { (p: UnsafeRawBufferPointer) in
                    while put < p.count {
                        let n = pwrite(fd, p.baseAddress! + put, p.count - put, offset + off_t(put))
                        if n < 0 { if errno == EINTR { continue }; throw PosixError(code: errno, op: "pwrite") }
                        put += n
                    }
                }
                return try ov.fstat(fd, ino: it.ino)
            }
            guard let r = FSWriteFileResult(bytesWritten: put, itemAttributes: attrs(e, of: it), freeSpace: FSFreeSpace.noUpdate) else {
                throw PosixError(code: EIO, op: "FSWriteFileResult")
            }
            return r
        }
    }

    // MARK: - FSVolume.XattrHandler

    func getXattr(named name: FSFileName, of item: FSItem, context: FSContext,
                  replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)
    {
        run(.getxattr, reply) {
            let it = try self.item(item)
            guard let n = name.string else { throw PosixError(code: EILSEQ, op: "xattr name") }
            let v = try ov.getXattr(table.path(it), name: n)
            guard let r = FSGetXattrResult(xattrValue: v) else { throw PosixError(code: EIO, op: "FSGetXattrResult") }
            return r
        }
    }

    func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy,
                  context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)
    {
        run(.setxattr, reply) {
            try requireWritable()
            let it = try self.item(item)
            guard let n = name.string else { throw PosixError(code: EILSEQ, op: "xattr name") }
            if n.hasPrefix(Overlay.hiddenAttrPrefix) { throw PosixError(code: EPERM, op: "reserved xattr \(n)") }
            let p = try table.path(it)
            try prepareChange(it, path: p)
            switch policy {
            case .delete:
                try ov.upper.removeXattr(p, name: n)
            case .alwaysSet, .mustCreate, .mustReplace:
                guard let value else { throw PosixError(code: EINVAL, op: "xattr value") }
                let opt: Int32 = policy == .mustCreate ? XATTR_CREATE : (policy == .mustReplace ? XATTR_REPLACE : 0)
                try ov.upper.setXattr(p, name: n, value: value, options: opt)
            @unknown default:
                throw PosixError(code: EINVAL, op: "xattr policy")
            }
            guard let r = FSSetXattrResult(freeSpace: FSFreeSpace.noUpdate) else { throw PosixError(code: EIO, op: "FSSetXattrResult") }
            return r
        }
    }

    func listXattrs(of item: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)
    {
        run(.listxattr, reply) {
            let it = try self.item(item)
            let names = try ov.listXattrs(table.path(it)).map { FSFileName(data: Data($0)) }
            guard let r = FSListXattrsResult(xattrNames: names) else { throw PosixError(code: EIO, op: "FSListXattrsResult") }
            return r
        }
    }
}
