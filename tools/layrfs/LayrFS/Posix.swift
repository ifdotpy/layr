// POSIX backend for the LayrFS passthrough volume.
// This file has no FSKit dependency, so selftest/ can compile and run it directly.

import Darwin
import Foundation

/// A POSIX error code thrown by the backend.
struct PosixError: Error, CustomStringConvertible {
    let code: Int32
    let op: String
    var description: String { "\(op): \(String(cString: strerror(code))) (\(code))" }
}

@inline(__always)
func check(_ rc: Int32, _ op: @autoclosure () -> String) throws {
    if rc < 0 { throw PosixError(code: errno, op: op()) }
}

// Object types from <sys/vnode.h> (enum vtype).
enum VType: UInt32 {
    case non = 0, reg = 1, dir = 2, blk = 3, chr = 4, lnk = 5, sock = 6, fifo = 7, bad = 8
}

/// One file system object, as read with getattrlist(2) or getattrlistbulk(2).
struct Entry {
    var name: [UInt8] = []
    var objType: UInt32 = 0
    var fileID: UInt64 = 0
    var parentID: UInt64 = 0
    var perm: UInt32 = 0          // permission bits only (07777)
    var uid: UInt32 = 0
    var gid: UInt32 = 0
    var flags: UInt32 = 0
    var linkCount: UInt32 = 1
    var size: UInt64 = 0
    var allocSize: UInt64 = 0
    var birthTime = timespec()
    var modifyTime = timespec()
    var changeTime = timespec()
    var accessTime = timespec()
    var backupTime = timespec()
    var addedTime = timespec()
    var hasAddedTime = false
    var error: UInt32 = 0

    var vtype: VType { VType(rawValue: objType) ?? .bad }

    /// st_mode style value: type bits plus permission bits.
    var fullMode: UInt32 {
        let t: UInt32
        switch vtype {
        case .reg: t = UInt32(S_IFREG)
        case .dir: t = UInt32(S_IFDIR)
        case .lnk: t = UInt32(S_IFLNK)
        case .blk: t = UInt32(S_IFBLK)
        case .chr: t = UInt32(S_IFCHR)
        case .sock: t = UInt32(S_IFSOCK)
        case .fifo: t = UInt32(S_IFIFO)
        default: t = 0
        }
        return t | (perm & 0o7777)
    }

    var nameString: String { String(decoding: name, as: UTF8.self) }
}

// Attribute bits from <sys/attr.h>. Declared here so the values are UInt32 without import surprises.
enum AttrBits {
    static let bitmapCount: UInt16 = 5
    // common
    static let cmnName: UInt32 = 0x0000_0001
    static let cmnObjType: UInt32 = 0x0000_0008
    static let cmnCrTime: UInt32 = 0x0000_0200
    static let cmnModTime: UInt32 = 0x0000_0400
    static let cmnChgTime: UInt32 = 0x0000_0800
    static let cmnAccTime: UInt32 = 0x0000_1000
    static let cmnBkupTime: UInt32 = 0x0000_2000
    static let cmnOwnerID: UInt32 = 0x0000_8000
    static let cmnGrpID: UInt32 = 0x0001_0000
    static let cmnAccessMask: UInt32 = 0x0002_0000
    static let cmnFlags: UInt32 = 0x0004_0000
    static let cmnFileID: UInt32 = 0x0200_0000
    static let cmnParentID: UInt32 = 0x0400_0000
    static let cmnAddedTime: UInt32 = 0x1000_0000
    static let cmnError: UInt32 = 0x2000_0000
    static let cmnReturnedAttrs: UInt32 = 0x8000_0000
    // directory
    static let dirLinkCount: UInt32 = 0x0000_0001
    static let dirAllocSize: UInt32 = 0x0000_0008
    static let dirDataLength: UInt32 = 0x0000_0020
    // file
    static let fileLinkCount: UInt32 = 0x0000_0001
    static let fileTotalSize: UInt32 = 0x0000_0002
    static let fileAllocSize: UInt32 = 0x0000_0004

    static let commonSet: UInt32 = cmnReturnedAttrs | cmnName | cmnObjType | cmnCrTime | cmnModTime
        | cmnChgTime | cmnAccTime | cmnBkupTime | cmnOwnerID | cmnGrpID | cmnAccessMask | cmnFlags
        | cmnFileID | cmnParentID | cmnAddedTime
    static let dirSet: UInt32 = dirLinkCount | dirAllocSize | dirDataLength
    static let fileSet: UInt32 = fileLinkCount | fileTotalSize | fileAllocSize

    static func list(bulk: Bool) -> attrlist {
        var a = attrlist()
        a.bitmapcount = bitmapCount
        a.commonattr = commonSet | (bulk ? cmnError : 0)
        a.dirattr = dirSet
        a.fileattr = fileSet
        return a
    }
}

/// Parses one attribute group (the layout of getattrlist(2) without FSOPT_PACK_INVAL_ATTRS).
/// `base` points at the leading u_int32_t length field.
func parseAttrGroup(_ base: UnsafeRawPointer) -> Entry {
    var e = Entry()
    var p = base + 4  // skip length
    // attribute_set_t: common, vol, dir, file, fork
    let rCommon = p.loadUnaligned(as: UInt32.self)
    let rDir = (p + 8).loadUnaligned(as: UInt32.self)
    let rFile = (p + 12).loadUnaligned(as: UInt32.self)
    p += 20

    func u32() -> UInt32 { let v = p.loadUnaligned(as: UInt32.self); p += 4; return v }
    func u64() -> UInt64 { let v = p.loadUnaligned(as: UInt64.self); p += 8; return v }
    func ts() -> timespec {
        let s = p.loadUnaligned(as: Int.self)
        let n = (p + 8).loadUnaligned(as: Int.self)
        p += 16
        return timespec(tv_sec: s, tv_nsec: n)
    }

    if rCommon & AttrBits.cmnError != 0 { e.error = u32() }
    if rCommon & AttrBits.cmnName != 0 {
        let refBase = p
        let off = p.loadUnaligned(as: Int32.self)
        let len = (p + 4).loadUnaligned(as: UInt32.self)
        p += 8
        if len > 0 {
            let s = (refBase + Int(off)).assumingMemoryBound(to: UInt8.self)
            // len includes the trailing NUL.
            e.name = Array(UnsafeBufferPointer(start: s, count: Int(len) - 1))
        }
    }
    if rCommon & AttrBits.cmnObjType != 0 { e.objType = u32() }
    if rCommon & AttrBits.cmnCrTime != 0 { e.birthTime = ts() }
    if rCommon & AttrBits.cmnModTime != 0 { e.modifyTime = ts() }
    if rCommon & AttrBits.cmnChgTime != 0 { e.changeTime = ts() }
    if rCommon & AttrBits.cmnAccTime != 0 { e.accessTime = ts() }
    if rCommon & AttrBits.cmnBkupTime != 0 { e.backupTime = ts() }
    if rCommon & AttrBits.cmnOwnerID != 0 { e.uid = u32() }
    if rCommon & AttrBits.cmnGrpID != 0 { e.gid = u32() }
    if rCommon & AttrBits.cmnAccessMask != 0 { e.perm = u32() & 0o7777 }
    if rCommon & AttrBits.cmnFlags != 0 { e.flags = u32() }
    if rCommon & AttrBits.cmnFileID != 0 { e.fileID = u64() }
    if rCommon & AttrBits.cmnParentID != 0 { e.parentID = u64() }
    if rCommon & AttrBits.cmnAddedTime != 0 { e.addedTime = ts(); e.hasAddedTime = true }
    if !e.hasAddedTime { e.addedTime = e.birthTime }

    if rDir != 0 {
        if rDir & AttrBits.dirLinkCount != 0 { e.linkCount = u32() }
        if rDir & AttrBits.dirAllocSize != 0 { e.allocSize = u64() }
        if rDir & AttrBits.dirDataLength != 0 { e.size = u64() }
    }
    if rFile != 0 {
        if rFile & AttrBits.fileLinkCount != 0 { e.linkCount = u32() }
        if rFile & AttrBits.fileTotalSize != 0 { e.size = u64() }
        if rFile & AttrBits.fileAllocSize != 0 { e.allocSize = u64() }
    }
    return e
}

/// The passthrough backend: all paths are relative to `rootFD`.
final class Backend {
    let rootPath: String
    let rootFD: Int32

    init(rootPath: String) throws {
        self.rootPath = rootPath
        let fd = open(rootPath, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        if fd < 0 { throw PosixError(code: errno, op: "open root \(rootPath)") }
        rootFD = fd
    }

    deinit { close(rootFD) }

    private static let singleBufSize = 4096

    /// Attributes of the object at `rel` (lstat semantics: symlinks are not followed).
    func stat(_ rel: String) throws -> Entry {
        var al = AttrBits.list(bulk: false)
        let buf = UnsafeMutableRawPointer.allocate(byteCount: Self.singleBufSize, alignment: 8)
        defer { buf.deallocate() }
        let rc = getattrlistat(rootFD, rel, &al, buf, Self.singleBufSize, UInt(FSOPT_NOFOLLOW))
        try check(rc, "getattrlistat \(rel)")
        var e = parseAttrGroup(UnsafeRawPointer(buf))
        if e.vtype == .dir { fixDirLinkCount(&e, dirFD: rootFD, rel: rel) }
        return e
    }

    /// ATTR_DIR_LINKCOUNT is 1 on APFS while stat(2) reports 2 + subdirectories. Use stat's value so
    /// the mount shows the same st_nlink as the backing directory (one extra syscall per directory).
    private func fixDirLinkCount(_ e: inout Entry, dirFD: Int32, rel: String) {
        var s = Darwin.stat()
        if fstatat(dirFD, rel, &s, AT_SYMLINK_NOFOLLOW) == 0 { e.linkCount = UInt32(s.st_nlink) }
    }

    /// Attributes of an open file descriptor.
    func fstat(_ fd: Int32) throws -> Entry {
        var al = AttrBits.list(bulk: false)
        let buf = UnsafeMutableRawPointer.allocate(byteCount: Self.singleBufSize, alignment: 8)
        defer { buf.deallocate() }
        let rc = fgetattrlist(fd, &al, buf, Self.singleBufSize, 0)
        try check(rc, "fgetattrlist")
        var e = parseAttrGroup(UnsafeRawPointer(buf))
        if e.vtype == .dir {
            var s = Darwin.stat()
            if Darwin.fstat(fd, &s) == 0 { e.linkCount = UInt32(s.st_nlink) }
        }
        return e
    }

    /// All entries of the directory at `rel`, read with getattrlistbulk(2).
    /// The directory descriptor is closed before return (no descriptor is kept between calls).
    func list(_ rel: String) throws -> [Entry] {
        let dfd = openat(rootFD, rel, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        if dfd < 0 { throw PosixError(code: errno, op: "openat dir \(rel)") }
        defer { close(dfd) }
        var al = AttrBits.list(bulk: true)
        let size = 256 * 1024
        let buf = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 8)
        defer { buf.deallocate() }
        var out: [Entry] = []
        while true {
            let n = getattrlistbulk(dfd, &al, buf, size, 0)
            if n < 0 { throw PosixError(code: errno, op: "getattrlistbulk \(rel)") }
            if n == 0 { break }
            var p = UnsafeRawPointer(buf)
            for _ in 0..<n {
                let len = p.loadUnaligned(as: UInt32.self)
                var e = parseAttrGroup(p)
                if e.error == 0 && !e.name.isEmpty {
                    if e.vtype == .dir { fixDirLinkCount(&e, dirFD: dfd, rel: e.nameString) }
                    out.append(e)
                }
                p += Int(len)
            }
        }
        return out
    }

    func openFile(_ rel: String, flags: Int32, mode: mode_t = 0) throws -> Int32 {
        let fd = openat(rootFD, rel, flags | O_CLOEXEC | O_NOFOLLOW, mode)
        if fd < 0 { throw PosixError(code: errno, op: "openat \(rel)") }
        return fd
    }

    func createFile(_ rel: String, mode: mode_t) throws {
        let fd = try openFile(rel, flags: O_CREAT | O_EXCL | O_WRONLY, mode: mode)
        close(fd)
    }

    func mkdir(_ rel: String, mode: mode_t) throws {
        try check(mkdirat(rootFD, rel, mode), "mkdirat \(rel)")
    }

    func symlink(_ rel: String, target: String) throws {
        try check(symlinkat(target, rootFD, rel), "symlinkat \(rel)")
    }

    func link(from src: String, to dst: String) throws {
        try check(linkat(rootFD, src, rootFD, dst, 0), "linkat \(src) -> \(dst)")
    }

    func readlink(_ rel: String) throws -> [UInt8] {
        var buf = [UInt8](repeating: 0, count: Int(PATH_MAX) + 1)
        let n = buf.withUnsafeMutableBufferPointer { b in
            b.baseAddress!.withMemoryRebound(to: CChar.self, capacity: b.count) {
                readlinkat(rootFD, rel, $0, b.count)
            }
        }
        if n < 0 { throw PosixError(code: errno, op: "readlinkat \(rel)") }
        return Array(buf[0..<n])
    }

    func unlink(_ rel: String, isDir: Bool) throws {
        try check(unlinkat(rootFD, rel, isDir ? AT_REMOVEDIR : 0), "unlinkat \(rel)")
    }

    func rename(_ src: String, _ dst: String) throws {
        try check(renameat(rootFD, src, rootFD, dst), "renameat \(src) -> \(dst)")
    }

    func chmod(_ rel: String, mode: mode_t) throws {
        try check(fchmodat(rootFD, rel, mode, AT_SYMLINK_NOFOLLOW), "fchmodat \(rel)")
    }

    func chown(_ rel: String, uid: uid_t, gid: gid_t) throws {
        try check(fchownat(rootFD, rel, uid, gid, AT_SYMLINK_NOFOLLOW), "fchownat \(rel)")
    }

    func truncate(_ rel: String, fd: Int32?, size: UInt64) throws {
        if let fd {
            try check(ftruncate(fd, off_t(size)), "ftruncate \(rel)")
            return
        }
        let tfd = try openFile(rel, flags: O_WRONLY)
        defer { close(tfd) }
        try check(ftruncate(tfd, off_t(size)), "ftruncate \(rel)")
    }

    /// Sets access and modify times. Pass nil to keep a time.
    func utimes(_ rel: String, atime: timespec?, mtime: timespec?) throws {
        let omit = timespec(tv_sec: 0, tv_nsec: Int(UTIME_OMIT))
        var times = [atime ?? omit, mtime ?? omit]
        try check(utimensat(rootFD, rel, &times, AT_SYMLINK_NOFOLLOW), "utimensat \(rel)")
    }

    /// Sets birth time, backup time and BSD flags with setattrlistat(2).
    func setCommon(_ rel: String, birth: timespec?, backup: timespec?, flags: UInt32?) throws {
        var al = attrlist()
        al.bitmapcount = AttrBits.bitmapCount
        var data = [UInt8]()
        func append<T>(_ v: T) { withUnsafeBytes(of: v) { data.append(contentsOf: $0) } }
        if let birth { al.commonattr |= AttrBits.cmnCrTime; append(birth) }
        if let backup { al.commonattr |= AttrBits.cmnBkupTime; append(backup) }
        if let flags { al.commonattr |= AttrBits.cmnFlags; append(flags) }
        if al.commonattr == 0 { return }
        let rc = data.withUnsafeMutableBytes { b in
            setattrlistat(rootFD, rel, &al, b.baseAddress, b.count, UInt32(FSOPT_NOFOLLOW))
        }
        try check(rc, "setattrlistat \(rel)")
    }

    // MARK: xattr (paths are absolute because the *xattr calls have no *at form)

    func absolute(_ rel: String) -> String { rel == "." ? rootPath : rootPath + "/" + rel }

    func getXattr(_ rel: String, name: String) throws -> Data {
        let path = absolute(rel)
        while true {
            let size = getxattr(path, name, nil, 0, 0, XATTR_NOFOLLOW)
            if size < 0 { throw PosixError(code: errno, op: "getxattr \(name)") }
            var data = Data(count: size)
            let n = data.withUnsafeMutableBytes { getxattr(path, name, $0.baseAddress, size, 0, XATTR_NOFOLLOW) }
            if n < 0 {
                if errno == ERANGE { continue }  // grew between the two calls
                throw PosixError(code: errno, op: "getxattr \(name)")
            }
            data.count = n
            return data
        }
    }

    func setXattr(_ rel: String, name: String, value: Data, options: Int32) throws {
        let path = absolute(rel)
        let rc = value.withUnsafeBytes { setxattr(path, name, $0.baseAddress, value.count, 0, options | XATTR_NOFOLLOW) }
        try check(rc, "setxattr \(name)")
    }

    func removeXattr(_ rel: String, name: String) throws {
        try check(removexattr(absolute(rel), name, XATTR_NOFOLLOW), "removexattr \(name)")
    }

    func listXattrs(_ rel: String) throws -> [[UInt8]] {
        let path = absolute(rel)
        while true {
            let size = listxattr(path, nil, 0, XATTR_NOFOLLOW)
            if size < 0 { throw PosixError(code: errno, op: "listxattr") }
            if size == 0 { return [] }
            var buf = [UInt8](repeating: 0, count: size)
            let n = buf.withUnsafeMutableBytes { b in
                listxattr(path, b.baseAddress!.assumingMemoryBound(to: CChar.self), size, XATTR_NOFOLLOW)
            }
            if n < 0 {
                if errno == ERANGE { continue }
                throw PosixError(code: errno, op: "listxattr")
            }
            return buf[0..<n].split(separator: 0).map { Array($0) }
        }
    }

    func statfs() throws -> Darwin.statfs {
        var s = Darwin.statfs()
        try check(fstatfs(rootFD, &s), "fstatfs")
        return s
    }
}
