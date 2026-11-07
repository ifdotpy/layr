// LayrFS passthrough volume: macOS 27 (FSKit V3) handler protocols over a directory.

import Darwin
import Foundation
import FSKit
import ObjectiveC

final class LayrVolume: FSVolume, FSVolume.Handler, FSVolume.PathConfOperations,
    FSVolume.OpenCloseHandler, FSVolume.ReadWriteHandler, FSVolume.XattrHandler,
    FSVolume.DataCacheHandler
{
    let resource: FSPathURLResource
    let backend: Backend
    let table: ItemTable
    let config: Config
    let readOnly: Bool
    let rootIno: UInt64
    /// Set when config index=memory: metadata calls are served from memory.
    let index: MemIndex?
    private let scoped: Bool
    private var probedContext = false
    private let probeLock = NSLock()

    init(resource: FSPathURLResource, config: Config, readOnly: Bool, volumeID: UUID) throws {
        self.resource = resource
        self.config = config
        self.readOnly = readOnly
        let url = resource.url
        scoped = url.startAccessingSecurityScopedResource()
        backend = try Backend(rootPath: url.path)
        let rootEntry = try backend.stat(".")
        rootIno = rootEntry.fileID
        table = ItemTable(root: LayrItem(ino: rootEntry.fileID, type: .directory, parent: nil, name: ""))
        var indexNote: [String: Any] = ["mode": "disk"]
        if config.indexMemory {
            var bs = MemIndex.BuildStats()
            index = try MemIndex(backend: backend, stats: &bs)
            indexNote = bs.json
            indexNote["mode"] = "memory"
            indexNote["case_sensitive"] = index!.caseSensitive
        } else {
            index = nil
        }
        LayrItem.fdBudget.configure()
        umask(0)  // create modes come from the caller; the kernel already applied its umask
        super.init(volumeID: FSVolume.Identifier(uuid: volumeID),
                   volumeName: FSFileName(string: url.lastPathComponent.isEmpty ? "layrfs" : url.lastPathComponent))
        Stats.shared.setNote("backing_path", url.path)
        Stats.shared.setNote("security_scoped", scoped)
        Stats.shared.setNote("read_only", readOnly)
        Stats.shared.setNote("config", config.asJSON)
        Stats.shared.setNote("index_build", indexNote)
        Stats.shared.setNote("fd_budget", LayrItem.fdBudget.json)
        fsLog.notice("layrfs index: \(String(describing: indexNote), privacy: .public)")
        Stats.shared.setNote("caller_identity_sdk", [
            "FSContext": "realUserID, effectiveUserID, realGroupID, effectiveGroupID (NSInteger); nothing else in FSKit 27.2 headers",
            "pid_or_audit_token_in_sdk": false,
        ])
        fsLog.notice("layrfs volume init path=\(url.path, privacy: .public) scoped=\(self.scoped) rootIno=\(rootEntry.fileID) datacache=\(config.dataCache.rawValue, privacy: .public)")
    }

    deinit {
        if scoped { resource.url.stopAccessingSecurityScopedResource() }
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

    /// Times `body`, records the op, and replies once.
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
        if ino == rootIno { return .rootDirectory }
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

    /// Populates every attribute: FSKit caches all populated attributes (FSVolumeHandlerResult.h).
    private func attributes(_ e: Entry) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.type = itemType(e)
        a.mode = e.fullMode
        a.uid = e.uid
        a.gid = e.gid
        a.linkCount = e.linkCount
        a.flags = e.flags
        a.size = e.size
        a.allocSize = e.allocSize
        a.fileID = id(e.fileID)
        a.parentID = e.fileID == rootIno ? .parentOfRoot : id(e.parentID)
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

    private func item(_ i: FSItem) throws -> LayrItem {
        guard let it = i as? LayrItem else { throw PosixError(code: EINVAL, op: "foreign FSItem") }
        return it
    }

    private func nameString(_ n: FSFileName) throws -> String {
        guard let s = n.string, !s.isEmpty, !s.contains("/") else { throw PosixError(code: EILSEQ, op: "file name") }
        return s
    }

    private func attrsOf(_ it: LayrItem) throws -> Entry {
        if let e = try it.withFD(write: false, { try backend.fstat($0) }) { return e }
        return try backend.stat(table.path(it))
    }

    /// Attributes from the index when it has the item, else from disk.
    private func entryFor(_ it: LayrItem) throws -> Entry {
        if let index, !it.unlinked, let e = index.entry(it.ino) { return e }
        return try attrsOf(it)
    }

    private func modesString(_ m: FSVolume.OpenModes) -> String {
        var s: [String] = []
        if m.contains(.read) { s.append("read") }
        if m.contains(.write) { s.append("write") }
        return s.isEmpty ? "none" : s.joined(separator: "|")
    }

    private func cacheModeString(_ m: FSVolume.DataCacheMode) -> String {
        switch m {
        case .none: return "none"
        case .readWithCache: return "readWithCache"
        case .readWriteWithCache: return "readWriteWithCache"
        @unknown default: return "unknown(\(m.rawValue))"
        }
    }

    private func coherencyString(_ c: FSVolume.KernelCacheCoherencyType) -> String {
        switch c {
        case .noCache: return "noCache"
        case .readCache: return "readCache"
        case .writeThrough: return "writeThrough"
        case .writeBack: return "writeBack"
        @unknown default: return "unknown(\(c.rawValue))"
        }
    }

    /// Records, once per process, what an FSKit callback can see about its caller.
    private func probeCaller(_ ctx: FSContext, op: String) {
        probeLock.lock()
        if probedContext { probeLock.unlock(); return }
        probedContext = true
        probeLock.unlock()

        var info: [String: Any] = ["first_op": op]
        var cls: AnyClass? = object_getClass(ctx)
        var classes: [[String: Any]] = []
        while let c = cls, c != NSObject.self {
            var pc: UInt32 = 0, ic: UInt32 = 0
            var props: [String] = []
            var ivars: [String] = []
            if let pl = class_copyPropertyList(c, &pc) {
                for i in 0..<Int(pc) { props.append(String(cString: property_getName(pl[i]))) }
                free(pl)
            }
            if let il = class_copyIvarList(c, &ic) {
                for i in 0..<Int(ic) {
                    let n = ivar_getName(il[i]).map { String(cString: $0) } ?? "?"
                    let t = ivar_getTypeEncoding(il[i]).map { String(cString: $0) } ?? "?"
                    ivars.append("\(n):\(t)")
                }
                free(il)
            }
            classes.append(["class": NSStringFromClass(c), "properties": props, "ivars": ivars])
            cls = class_getSuperclass(c)
        }
        info["fscontext_runtime"] = classes
        info["fscontext_values"] = ["ruid": ctx.realUserID, "euid": ctx.effectiveUserID,
                                    "rgid": ctx.realGroupID, "egid": ctx.effectiveGroupID]
        // The XPC peer, if any, is the FSKit daemon, not the process that made the syscall.
        if let conn = NSXPCConnection.current() {
            info["xpc_current_connection"] = ["pid": Int(conn.processIdentifier),
                                              "euid": Int(conn.effectiveUserIdentifier),
                                              "asid": Int(conn.auditSessionIdentifier),
                                              "service": conn.serviceName ?? ""]
        } else {
            info["xpc_current_connection"] = "nil"
        }
        Stats.shared.setNote("caller_identity_runtime", info)
        fsLog.notice("layrfs caller probe: \(String(describing: info), privacy: .public)")
    }

    private func logOpen(_ it: LayrItem, event: String, modes: String, cacheMode: String = "",
                         granted: String = "", ctx: FSContext?, error: Int32)
    {
        guard config.logOpens else { return }
        let p = (try? table.path(it)) ?? "<removed ino \(it.ino)>"
        Stats.shared.recordOpen(OpenRecord(
            t: 0, event: event, path: p, modes: modes, cacheMode: cacheMode, granted: granted,
            ruid: ctx?.realUserID ?? -1, euid: ctx?.effectiveUserID ?? -1,
            rgid: ctx?.realGroupID ?? -1, egid: ctx?.effectiveGroupID ?? -1, error: error))
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
        c.caseFormat = .insensitiveCasePreserving  // matches the APFS default of the backing volume
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let t0 = nowNs()
        let r = FSStatFSResult(fileSystemTypeName: "layrfs")
        if let s = index?.statfs ?? (try? backend.statfs()) {
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

    var enableOpenUnlinkEmulation: Bool { false }  // an open item keeps its own fd

    func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void) {
        Stats.shared.setNote("activate_task_options", options.taskOptions)
        run(.activate, reply) {
            guard let r = FSActivateResult(rootItem: table.root) else { throw PosixError(code: EIO, op: "FSActivateResult") }
            return r
        }
    }

    func deactivateVolume(options: FSDeactivateOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        runVoid(.deactivate, reply) {
            Stats.shared.setNote("items_in_table_at_deactivate", table.count)
            Stats.shared.setNote("fd_budget", LayrItem.fdBudget.json)
            if let index { Stats.shared.setNote("index_nodes_at_deactivate", index.count) }
            Stats.shared.dump(reason: "deactivate")
        }
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        Stats.shared.setNote("mount_task_options", options.taskOptions)
        runVoid(.mount, reply) {}
    }

    func unmount(replyHandler reply: @escaping @Sendable () -> Void) {
        let t0 = nowNs()
        Stats.shared.record(.unmount, ns: nowNs() - t0)
        Stats.shared.dump(reason: "unmount")
        reply()
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        runVoid(.synchronize, reply) {}  // all writes are already in the backing file system
    }

    // MARK: - FSVolume.Handler: items

    func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "lookup")
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
                if let index {
                    e = try index.lookup(dir: dir.ino, name: n)
                } else {
                    e = try backend.stat(table.childPath(dir, n))
                }
                // The on-disk name, so case-insensitive lookups return the stored case.
                let diskName = e.name.isEmpty ? n : e.nameString
                found = table.resolve(e, type: itemType(e), parent: dir, name: diskName)
            }
            let outName = e.name.isEmpty ? name : FSFileName(data: Data(e.name))
            guard let r = FSLookupItemResult(foundItem: found, itemName: outName, itemAttributes: attributes(e)) else {
                throw PosixError(code: EIO, op: "FSLookupItemResult")
            }
            return r
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        let t0 = nowNs()
        guard let it = item as? LayrItem else { reply(nil); return }
        table.lock.lock()
        // Critical section: lookups take the same lock, so the item cannot be returned concurrently.
        let ran = it.tryReclaim { self.table.reclaimLocked(it) }
        table.lock.unlock()
        if ran { it.closeFD() }
        Stats.shared.record(.reclaim, ns: nowNs() - t0)
        reply(nil)
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext,
                       replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "getattr")
        run(.getattr, reply) {
            let it = try self.item(item)
            guard let r = FSGetAttributesResult(attributes: attributes(try entryFor(it))) else {
                throw PosixError(code: EIO, op: "FSGetAttributesResult")
            }
            return r
        }
    }

    /// Applies a set-attributes request. Returns the consumed attributes.
    private func apply(_ req: FSItem.SetAttributesRequest, to path: String, item it: LayrItem?,
                       skipMode: Bool = false) throws -> FSItem.Attribute
    {
        var used: FSItem.Attribute = []
        if !skipMode && req.isValid(.mode) {
            try backend.chmod(path, mode: mode_t(req.mode & 0o7777)); used.insert(.mode)
        }
        let wantUID = req.isValid(.uid), wantGID = req.isValid(.gid)
        if wantUID || wantGID {
            try backend.chown(path, uid: wantUID ? req.uid : uid_t.max, gid: wantGID ? req.gid : gid_t.max)
            if wantUID { used.insert(.uid) }
            if wantGID { used.insert(.gid) }
        }
        if req.isValid(.size) {
            if it?.type == .file || it == nil {
                if let it, let done = try it.withFD(write: true, { fd -> Bool in
                    try backend.truncate(path, fd: fd, size: req.size); return true
                }), done {
                } else {
                    try backend.truncate(path, fd: nil, size: req.size)
                }
            }
            used.insert(.size)  // ignored for directories and symlinks, as the protocol asks
        }
        let wantA = req.isValid(.accessTime), wantM = req.isValid(.modifyTime)
        if wantA || wantM {
            try backend.utimes(path, atime: wantA ? req.accessTime : nil, mtime: wantM ? req.modifyTime : nil)
            if wantA { used.insert(.accessTime) }
            if wantM { used.insert(.modifyTime) }
        }
        let wantB = req.isValid(.birthTime), wantK = req.isValid(.backupTime), wantF = req.isValid(.flags)
        if wantB || wantK || wantF {
            try backend.setCommon(path, birth: wantB ? req.birthTime : nil, backup: wantK ? req.backupTime : nil,
                                  flags: wantF ? req.flags : nil)
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
        probeCaller(context, op: "setattr")
        run(.setattr, reply) {
            try requireWritable()
            let it = try self.item(item)
            let p = try table.path(it)
            _ = try apply(newAttributes, to: p, item: it)
            let e = try attrsOf(it)
            index?.update(e)
            guard let r = FSSetAttributesResult(attributes: attributes(e), freeSpace: FSFreeSpace.noUpdate) else {
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
        probeCaller(context, op: "create")
        run(type == .directory ? .mkdir : .create, reply) {
            try requireWritable()
            let dir = try item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            let mode = mode_t(newAttributes.isValid(.mode) ? newAttributes.mode & 0o7777 : (type == .directory ? 0o755 : 0o644))
            switch type {
            case .file: try backend.createFile(p, mode: mode)
            case .directory: try backend.mkdir(p, mode: mode)
            default: throw PosixError(code: ENOTSUP, op: "create type \(type.rawValue)")
            }
            dropSnapshot(dir)
            let rest = try apply(newAttributes, to: p, item: nil, skipMode: true)
            newAttributes.consumedAttributes = rest.union(newAttributes.isValid(.mode) ? .mode : [])
            let e: Entry, d: Entry
            if let index {
                (e, d) = try index.didCreate(dir: dir.ino, dirPath: table.path(dir), name: n, backend: backend)
            } else {
                e = try backend.stat(p)
                d = try attrsOf(dir)
            }
            let it = table.resolve(e, type: itemType(e), parent: dir, name: n)
            guard let r = FSCreateItemResult(newItem: it, newItemName: name, newItemAttributes: attributes(e),
                                             directoryAttributes: attributes(d), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateItemResult") }
            return r
        }
    }

    func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName, context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "symlink")
        run(.symlink, reply) {
            try requireWritable()
            let dir = try item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            guard let target = contents.string else { throw PosixError(code: EILSEQ, op: "symlink target") }
            try backend.symlink(p, target: target)
            dropSnapshot(dir)
            _ = try? apply(newAttributes, to: p, item: nil)
            let e: Entry, d: Entry
            if let index {
                (e, d) = try index.didCreate(dir: dir.ino, dirPath: table.path(dir), name: n, backend: backend)
            } else {
                e = try backend.stat(p)
                d = try attrsOf(dir)
            }
            let it = table.resolve(e, type: .symlink, parent: dir, name: n)
            guard let r = FSCreateSymlinkResult(newItem: it, newItemName: name, newItemAttributes: attributes(e),
                                                directoryAttributes: attributes(d), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateSymlinkResult") }
            return r
        }
    }

    func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "link")
        run(.link, reply) {
            try requireWritable()
            let it = try self.item(item)
            let dir = try self.item(directory)
            let n = try nameString(name)
            let dst = try table.childPath(dir, n)
            try backend.link(from: table.path(it), to: dst)
            dropSnapshot(dir)
            let e: Entry, d: Entry
            if let index {
                (e, d) = try index.didCreate(dir: dir.ino, dirPath: table.path(dir), name: n, backend: backend)
            } else {
                e = try backend.stat(dst)
                d = try attrsOf(dir)
            }
            guard let r = FSCreateLinkResult(linkName: name, linkAttributes: attributes(e),
                                             directoryAttributes: attributes(d), freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSCreateLinkResult") }
            return r
        }
    }

    func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
                    to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?,
                    context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "rename")
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
            if over != nil { overEntry = try? backend.stat(dp) }
            try backend.rename(sp, dp)
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
            if let index {
                try index.didRename(src: src.ino, srcPath: table.path(src), srcName: sn,
                                    dst: dst.ino, dstPath: table.path(dst), dstName: dn, backend: backend)
            }
            let e = try backend.stat(dp)
            let sa = try attrsOf(src)
            let da = src === dst ? sa : try attrsOf(dst)
            guard let r = FSRenameItemResult(newName: destinationName, renamedItemAttributes: attributes(e),
                                             sourceDirectoryAttributes: attributes(sa),
                                             destinationDirectoryAttributes: attributes(da),
                                             overItemAttributes: overEntry.map { attributes($0) },
                                             freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSRenameItemResult") }
            return r
        }
    }

    func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "remove")
        run(.remove, reply) {
            try requireWritable()
            let it = try self.item(item)
            let dir = try self.item(directory)
            let n = try nameString(name)
            let p = try table.childPath(dir, n)
            var pre = try backend.stat(p)
            try backend.unlink(p, isDir: it.type == .directory)
            if let index { try index.didRemove(dir: dir.ino, dirPath: table.path(dir), name: n, backend: backend) }
            pre.linkCount = (it.type == .directory || pre.linkCount <= 1) ? 0 : pre.linkCount - 1
            table.lock.lock()
            dir.snapshot = nil
            if pre.linkCount == 0 { it.unlinked = true; table.detachLocked(it) } else { it.stalePath = true }
            table.lock.unlock()
            let d = try attrsOf(dir)
            guard let r = FSRemoveItemResult(itemAttributes: attributes(pre), directoryAttributes: attributes(d),
                                             freeSpace: FSFreeSpace.noUpdate)
            else { throw PosixError(code: EIO, op: "FSRemoveItemResult") }
            return r
        }
    }

    func readSymbolicLink(_ item: FSItem, context: FSContext,
                          replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "readlink")
        run(.readlink, reply) {
            let it = try self.item(item)
            let target: [UInt8], e: Entry
            if let index, let t = index.symlinkTarget(it.ino), let ie = index.entry(it.ino) {
                target = t
                e = ie
            } else {
                let p = try table.path(it)
                target = try backend.readlink(p)
                e = try backend.stat(p)
            }
            guard let r = FSReadSymlinkResult(contents: FSFileName(data: Data(target)), symlinkAttributes: attributes(e))
            else { throw PosixError(code: EIO, op: "FSReadSymlinkResult") }
            return r
        }
    }

    /// Cookie c means "start at index c" of the snapshot. The snapshot is read once per enumeration
    /// with getattrlistbulk and the directory descriptor is closed at once (no leak, no telldir).
    func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
                            attributes attrReq: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
                            context: FSContext,
                            replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "enumerate")
        if config.nullEnumerateProbe {
            // XPC floor probe: no lock, no syscall, no entries. Every directory looks empty.
            run(.enumerate, reply) {
                guard let r = FSEnumerateDirectoryResult(verifier: 1) else { throw PosixError(code: EIO, op: "FSEnumerateDirectoryResult") }
                return r
            }
            return
        }
        if let index {
            run(.enumerate, reply) { try enumerateFromIndex(index, directory, cookie, attrReq != nil, packer) }
            return
        }
        run(.enumerate, reply) {
            let dir = try item(directory)
            let withAttrs = attrReq != nil
            table.lock.lock()
            var snap = dir.snapshot
            let parent = dir.parent ?? table.root
            table.lock.unlock()

            if cookie == .initial || snap == nil || snap!.verifier != verifier.rawValue || snap!.withAttributes != withAttrs {
                let p = try table.path(dir)
                let d = try backend.stat(p)
                var v = (UInt64(bitPattern: Int64(d.modifyTime.tv_sec)) &* 1_000_000_007)
                    ^ UInt64(bitPattern: Int64(d.modifyTime.tv_nsec))
                    ^ (UInt64(bitPattern: Int64(d.changeTime.tv_nsec)) << 32)
                if v == 0 { v = 1 }
                var entries = try backend.list(p)
                if !withAttrs {
                    var dot = Entry(); dot.name = Array(".".utf8); dot.objType = VType.dir.rawValue; dot.fileID = dir.ino
                    var dotdot = Entry(); dotdot.name = Array("..".utf8); dotdot.objType = VType.dir.rawValue; dotdot.fileID = parent.ino
                    entries.insert(contentsOf: [dot, dotdot], at: 0)
                }
                snap = DirSnapshot(verifier: v, withAttributes: withAttrs, entries: entries)
            }
            let s = snap!
            let start = Int(cookie.rawValue)
            if start > s.entries.count {
                throw FSError(.invalidDirectoryCookie)
            }
            var i = start
            while i < s.entries.count {
                let e = s.entries[i]
                let ok = packer.packEntry(name: FSFileName(data: Data(e.name)), itemType: itemType(e), itemID: id(e.fileID),
                                          nextCookie: FSDirectoryCookie(UInt64(i + 1)),
                                          attributes: withAttrs ? attributes(e) : nil)
                if !ok { break }
                i += 1
            }
            table.lock.lock()
            dir.snapshot = i < s.entries.count ? s : nil  // keep only while the enumeration is unfinished
            table.lock.unlock()
            guard let r = FSEnumerateDirectoryResult(verifier: s.verifier) else {
                throw PosixError(code: EIO, op: "FSEnumerateDirectoryResult")
            }
            return r
        }
    }

    /// Index mode: the cookie is the position in the sorted child list ("." and ".." first when no
    /// attributes are requested); the verifier is the directory version, which every change increments.
    private func enumerateFromIndex(_ index: MemIndex, _ directory: FSItem, _ cookie: FSDirectoryCookie,
                                    _ withAttrs: Bool, _ packer: FSDirectoryEntryPacker) throws -> FSEnumerateDirectoryResult
    {
        let dir = try item(directory)
        let (gen, kids) = try index.children(dir.ino)
        let off = withAttrs ? 0 : 2
        let total = kids.count + off
        let start = Int(cookie.rawValue)
        if start > total { throw FSError(.invalidDirectoryCookie) }
        var i = start
        packing: while i < total {
            if i < off {
                table.lock.lock()
                let ino = i == 0 ? dir.ino : (dir.parent ?? table.root).ino
                table.lock.unlock()
                let ok = packer.packEntry(name: FSFileName(string: i == 0 ? "." : ".."), itemType: .directory,
                                          itemID: id(ino), nextCookie: FSDirectoryCookie(UInt64(i + 1)), attributes: nil)
                if !ok { break packing }
                i += 1
                continue
            }
            let end = min(total, i + 256)
            let slice = kids[(i - off)..<(end - off)]
            let entries = index.entries(slice)
            for e in entries {
                if let e {
                    let ok = packer.packEntry(name: FSFileName(data: Data(e.name)), itemType: itemType(e), itemID: id(e.fileID),
                                              nextCookie: FSDirectoryCookie(UInt64(i + 1)),
                                              attributes: withAttrs ? attributes(e) : nil)
                    if !ok { break packing }
                }
                i += 1
            }
        }
        guard let r = FSEnumerateDirectoryResult(verifier: gen) else {
            throw PosixError(code: EIO, op: "FSEnumerateDirectoryResult")
        }
        return r
    }

    // MARK: - FSVolume.OpenCloseHandler (used when datacache=off)

    var isOpenCloseInhibited: Bool { !config.openClose }

    private func doOpen(_ it: LayrItem, modes: FSVolume.OpenModes) throws {
        if it.type != .file { return }
        if modes.contains(.write) { try requireWritable() }
        try it.ensureOpen(path: table.path(it), backend: backend, write: modes.contains(.write))
    }

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                  replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        probeCaller(context, op: "open")
        guard let it = item as? LayrItem else { reply(fs_errorForPOSIXError(EINVAL)); return }
        runVoid(.open, { e in
            self.logOpen(it, event: "open", modes: self.modesString(modes), ctx: context, error: e.map { self.code($0) } ?? 0)
            reply(e)
        }) {
            try doOpen(it, modes: modes)
        }
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext,
                   replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        runVoid(.close, reply) {
            let it = try self.item(item)
            logOpen(it, event: "close", modes: modesString(modes), ctx: context, error: 0)
            if modes.isEmpty { it.closeFD() }  // `modes` are the modes that stay open
        }
    }

    // MARK: - FSVolume.DataCacheHandler (default; replaces OpenCloseHandler when not inhibited)

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
        probeCaller(context, op: "cacheOpen")
        let g = grant(cacheMode)
        guard let it = item as? LayrItem else { reply(nil, fs_errorForPOSIXError(EINVAL)); return }
        run(.open, { r, e in
            self.logOpen(it, event: "cacheOpen", modes: self.modesString(modes), cacheMode: self.cacheModeString(cacheMode),
                         granted: self.coherencyString(g), ctx: context, error: e.map { self.code($0) } ?? 0)
            reply(r, e)
        }) {
            try doOpen(it, modes: modes)
            return FSOpenItemResult(grantedCoherency: g)
        }
    }

    func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void) {
        let t0 = nowNs()
        if let it = item as? LayrItem {
            logOpen(it, event: "cacheClose", modes: "", ctx: context, error: 0)
            it.closeFD()
        }
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
                try it.ensureOpen(path: table.path(it), backend: backend, write: true)
            }
            logOpen(it, event: "cacheUpgrade", modes: "", cacheMode: cacheModeString(cacheMode),
                    granted: coherencyString(g), ctx: context, error: 0)
            return FSUpgradeItemResult(grantedCoherency: g)
        }
    }

    // MARK: - FSVolume.ReadWriteHandler

    /// Runs `body` with the item's open fd, or with a temporary fd when the item is not open.
    private func withIO<R>(_ it: LayrItem, write: Bool, _ body: (Int32) throws -> R) throws -> R {
        if let r = try it.withFD(write: write, body) { return r }
        // Index mode: keep a read descriptor per item (within the budget) instead of one open per read.
        if index != nil, !write, try it.openCached(path: table.path(it), backend: backend),
           let r = try it.withFD(write: false, body) { return r }
        Stats.shared.countTempOpen()
        let fd = try backend.openFile(table.path(it), flags: write ? O_WRONLY : O_RDONLY)
        defer { Darwin.close(fd) }
        return try body(fd)
    }

    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
              replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)
    {
        run(.read, reply) {
            let it = try self.item(item)
            var got = 0
            let fdEntry: Entry? = try withIO(it, write: false) { fd in
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
                return index == nil ? try backend.fstat(fd) : nil
            }
            let e = try fdEntry ?? entryFor(it)
            Stats.shared.addBytes(.read, got)
            guard let r = FSReadFileResult(bytesRead: got, itemAttributes: attributes(e)) else {
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
            var put = 0
            let e: Entry = try withIO(it, write: true) { fd in
                try contents.withUnsafeBytes { (p: UnsafeRawBufferPointer) in
                    while put < p.count {
                        let n = pwrite(fd, p.baseAddress! + put, p.count - put, offset + off_t(put))
                        if n < 0 { if errno == EINTR { continue }; throw PosixError(code: errno, op: "pwrite") }
                        put += n
                    }
                }
                return try backend.fstat(fd)
            }
            index?.update(e)
            guard let r = FSWriteFileResult(bytesWritten: put, itemAttributes: attributes(e), freeSpace: FSFreeSpace.noUpdate) else {
                throw PosixError(code: EIO, op: "FSWriteFileResult")
            }
            return r
        }
    }

    // MARK: - FSVolume.XattrHandler

    func getXattr(named name: FSFileName, of item: FSItem, context: FSContext,
                  replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "getxattr")
        run(.getxattr, reply) {
            let it = try self.item(item)
            guard let n = name.string else { throw PosixError(code: EILSEQ, op: "xattr name") }
            let v = try backend.getXattr(table.path(it), name: n)
            guard let r = FSGetXattrResult(xattrValue: v) else { throw PosixError(code: EIO, op: "FSGetXattrResult") }
            return r
        }
    }

    func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy,
                  context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "setxattr")
        run(.setxattr, reply) {
            try requireWritable()
            let it = try self.item(item)
            guard let n = name.string else { throw PosixError(code: EILSEQ, op: "xattr name") }
            let p = try table.path(it)
            switch policy {
            case .delete:
                try backend.removeXattr(p, name: n)
            case .alwaysSet, .mustCreate, .mustReplace:
                guard let value else { throw PosixError(code: EINVAL, op: "xattr value") }
                let opt: Int32 = policy == .mustCreate ? XATTR_CREATE : (policy == .mustReplace ? XATTR_REPLACE : 0)
                try backend.setXattr(p, name: n, value: value, options: opt)
            @unknown default:
                throw PosixError(code: EINVAL, op: "xattr policy")
            }
            if let index { _ = try? index.refresh(path: p, backend: backend) }  // ctime changed
            guard let r = FSSetXattrResult(freeSpace: FSFreeSpace.noUpdate) else { throw PosixError(code: EIO, op: "FSSetXattrResult") }
            return r
        }
    }

    func listXattrs(of item: FSItem, context: FSContext,
                    replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)
    {
        probeCaller(context, op: "listxattr")
        run(.listxattr, reply) {
            let it = try self.item(item)
            let names = try backend.listXattrs(table.path(it)).map { FSFileName(data: Data($0)) }
            guard let r = FSListXattrsResult(xattrNames: names) else { throw PosixError(code: EIO, op: "FSListXattrsResult") }
            return r
        }
    }
}
