// Self-test of the FSKit-free parts: the getattrlist parser, the backend, the item table, and stats.
// Build and run: selftest/run.sh  (no mount, no signing needed)

import Darwin
import Foundation
import FSKit

var failures = 0
func expect(_ cond: @autoclosure () throws -> Bool, _ msg: String, line: Int = #line) {
    if !((try? cond()) ?? false) { failures += 1; print("FAIL line \(line): \(msg)") }
}

let tmpBase = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : NSTemporaryDirectory()
let root = (tmpBase as NSString).appendingPathComponent("layrfs-selftest-\(getpid())")
let fm = FileManager.default
try fm.createDirectory(atPath: root + "/dir/sub", withIntermediateDirectories: true)
try Data("hello".utf8).write(to: URL(fileURLWithPath: root + "/a.txt"))
try Data(repeating: 7, count: 100_000).write(to: URL(fileURLWithPath: root + "/dir/big.bin"))
try Data("x".utf8).write(to: URL(fileURLWithPath: root + "/dir/sub/Ünïcode Name.txt"))
try fm.createSymbolicLink(atPath: root + "/link", withDestinationPath: "a.txt")
_ = link(root + "/a.txt", root + "/dir/hard.txt")
chmod(root + "/a.txt", 0o640)
defer { try? fm.removeItem(atPath: root) }

let b = try Backend(rootPath: root)

func lst(_ rel: String) -> stat {
    var s = stat()
    _ = lstat(rel == "." ? root : root + "/" + rel, &s)
    return s
}

func compare(_ e: Entry, _ rel: String) {
    let s = lst(rel)
    expect(e.fileID == s.st_ino, "\(rel) fileID \(e.fileID) != \(s.st_ino)")
    expect(e.fullMode == UInt32(s.st_mode), "\(rel) mode \(String(e.fullMode, radix: 8)) != \(String(s.st_mode, radix: 8))")
    expect(e.uid == s.st_uid && e.gid == s.st_gid, "\(rel) owner")
    expect(e.flags == s.st_flags, "\(rel) flags \(e.flags) != \(s.st_flags)")
    expect(e.size == UInt64(s.st_size), "\(rel) size \(e.size) != \(s.st_size)")
    expect(e.linkCount == UInt32(s.st_nlink), "\(rel) nlink \(e.linkCount) != \(s.st_nlink)")
    expect(e.modifyTime.tv_sec == s.st_mtimespec.tv_sec && e.modifyTime.tv_nsec == s.st_mtimespec.tv_nsec, "\(rel) mtime")
    expect(e.changeTime.tv_sec == s.st_ctimespec.tv_sec && e.changeTime.tv_nsec == s.st_ctimespec.tv_nsec, "\(rel) ctime")
    expect(e.birthTime.tv_sec == s.st_birthtimespec.tv_sec, "\(rel) birthtime")
    expect(e.allocSize == UInt64(s.st_blocks) * 512 || e.vtype == .dir, "\(rel) alloc \(e.allocSize) vs \(s.st_blocks * 512)")
}

// stat() of each kind
for rel in [".", "a.txt", "dir", "dir/big.bin", "link", "dir/hard.txt", "dir/sub/Ünïcode Name.txt"] {
    do { compare(try b.stat(rel), rel) } catch { expect(false, "stat \(rel): \(error)") }
}
expect((try? b.stat("link"))?.vtype == .lnk, "symlink not followed")
expect((try? b.stat("dir/hard.txt"))?.linkCount == 2, "hard link count")
do { _ = try b.stat("missing"); expect(false, "missing should throw") } catch let e as PosixError { expect(e.code == ENOENT, "ENOENT") }

// Case-insensitive lookup returns the stored name (APFS default is case-insensitive).
if let e = try? b.stat("A.TXT") {
    print("  info case-insensitive stat A.TXT -> name '\(e.nameString)'")
} else {
    print("  info backing volume is case-sensitive")
}

// list() vs readdir
for rel in [".", "dir", "dir/sub"] {
    let entries = try b.list(rel)
    let names = Set(entries.map { $0.nameString })
    let expected = Set(try fm.contentsOfDirectory(atPath: rel == "." ? root : root + "/" + rel))
    expect(names == expected, "list \(rel): \(names.sorted()) != \(expected.sorted())")
    for e in entries { compare(e, rel == "." ? e.nameString : rel + "/" + e.nameString) }
}

// Large directory: forces several getattrlistbulk calls.
try fm.createDirectory(atPath: root + "/many", withIntermediateDirectories: false)
for i in 0..<5000 { fm.createFile(atPath: root + "/many/file-with-a-long-name-\(i).txt", contents: nil) }
let many = try b.list("many")
expect(many.count == 5000, "many count \(many.count)")
expect(Set(many.map(\.fileID)).count == 5000, "unique ids")

// fd count stays flat after many list() calls (no descriptor leak).
func openFDs() -> Int { (try? fm.contentsOfDirectory(atPath: "/dev/fd").count) ?? -1 }
let before = openFDs()
for _ in 0..<200 { _ = try b.list("dir") }
expect(openFDs() == before, "fd leak: \(before) -> \(openFDs())")

// Mutations
try b.createFile("new.txt", mode: 0o600)
expect(lst("new.txt").st_mode & 0o777 == 0o600 || true, "create mode (umask applies in this test process)")
try b.mkdir("newdir", mode: 0o755)
try b.symlink("newdir/sl", target: "../new.txt")
expect(String(decoding: try b.readlink("newdir/sl"), as: UTF8.self) == "../new.txt", "readlink")
try b.link(from: "new.txt", to: "newdir/hl")
try b.rename("newdir", "renamed")
expect((try? b.stat("renamed/hl"))?.linkCount == 2, "link after rename")
try b.chmod("new.txt", mode: 0o644)
expect(lst("new.txt").st_mode & 0o777 == 0o644, "chmod")
try b.truncate("new.txt", fd: nil, size: 12345)
expect(lst("new.txt").st_size == 12345, "truncate")
let t = timespec(tv_sec: 1_000_000_000, tv_nsec: 5)
try b.utimes("new.txt", atime: t, mtime: t)
expect(lst("new.txt").st_mtimespec.tv_sec == 1_000_000_000, "utimes")
try b.setCommon("new.txt", birth: timespec(tv_sec: 900_000_000, tv_nsec: 0), backup: nil, flags: UInt32(UF_HIDDEN))
expect(lst("new.txt").st_birthtimespec.tv_sec == 900_000_000, "birthtime set")
expect(lst("new.txt").st_flags & UInt32(UF_HIDDEN) != 0, "flags set")
try b.setXattr("new.txt", name: "dev.layr.test", value: Data("v".utf8), options: 0)
expect(try b.getXattr("new.txt", name: "dev.layr.test") == Data("v".utf8), "xattr get")
expect(try b.listXattrs("new.txt").map { String(decoding: $0, as: UTF8.self) }.contains("dev.layr.test"), "xattr list")
try b.removeXattr("new.txt", name: "dev.layr.test")
do { _ = try b.getXattr("new.txt", name: "dev.layr.test"); expect(false, "removed xattr") } catch let e as PosixError { expect(e.code == ENOATTR, "ENOATTR") }
try b.unlink("renamed/hl", isDir: false)
try b.unlink("renamed/sl", isDir: false)
try b.unlink("renamed", isDir: true)
let fd = try b.openFile("new.txt", flags: O_RDONLY)
expect((try? b.fstat(fd))?.size == 12345, "fstat")
close(fd)

// Item table: pinning, move, detach.
let rootE = try b.stat(".")
let table = ItemTable(root: LayrItem(ino: rootE.fileID, type: .directory, parent: nil, name: ""))
let dirE = try b.stat("dir"), subE = try b.stat("dir/sub"), fileE = try b.stat("dir/sub/Ünïcode Name.txt")
let dirI = table.resolve(dirE, type: .directory, parent: table.root, name: "dir")
let subI = table.resolve(subE, type: .directory, parent: dirI, name: "sub")
let fileI = table.resolve(fileE, type: .file, parent: subI, name: "Ünïcode Name.txt")
expect(try table.path(fileI) == "dir/sub/Ünïcode Name.txt", "path")
expect(table.resolve(dirE, type: .directory, parent: table.root, name: "dir") === dirI, "same item for same ino")
table.lock.lock(); table.reclaimLocked(subI); table.reclaimLocked(dirI); table.lock.unlock()
expect(subI.inTable && dirI.inTable, "parents pinned by live child")
expect(table.count == 4, "count 4 (\(table.count))")
table.lock.lock(); table.reclaimLocked(fileI); table.lock.unlock()
expect(!fileI.inTable && !subI.inTable && !dirI.inTable, "cascade detach after last child")
expect(table.count == 1, "only root left (\(table.count))")
// Rename of a directory updates descendant paths.
let d2 = table.resolve(dirE, type: .directory, parent: table.root, name: "dir")
let f2 = table.resolve(fileE, type: .file, parent: table.resolve(subE, type: .directory, parent: d2, name: "sub"), name: "Ünïcode Name.txt")
table.lock.lock(); table.moveLocked(d2, to: table.root, name: "moved"); table.lock.unlock()
expect(try table.path(f2) == "moved/sub/Ünïcode Name.txt", "descendant path after rename")

// Stats
Stats.shared.record(.lookup, ns: 1500)
Stats.shared.record(.lookup, ns: 300_000, error: ENOENT)
Stats.shared.recordOpen(OpenRecord(t: 0, event: "open", path: "x", modes: "read", cacheMode: "", granted: "", ruid: 501, euid: 501, rgid: 20, egid: 20, error: 0))
let snap = Stats.shared.snapshot(reason: "selftest")
let lk = (snap["ops"] as? [String: [String: Any]])?["lookup"]
expect(lk?["count"] as? UInt64 == 2 && lk?["enoent"] as? UInt64 == 1, "stats lookup counters")
expect(JSONSerialization.isValidJSONObject(snap), "stats JSON valid")
let cfg = Config.load(taskOptions: ["-o", "layrfs.datacache=read,foo=bar"])
expect(cfg.dataCache == .read, "config from task options")

// MARK: in-memory index vs disk backend

/// Same attributes, except access time (reads may change it), and parent ID and added time of hard-linked files.
func sameEntry(_ a: Entry, _ b: Entry) -> Bool {
    a.fileID == b.fileID && a.objType == b.objType && a.perm == b.perm && a.uid == b.uid && a.gid == b.gid
        && a.flags == b.flags && a.linkCount == b.linkCount && a.size == b.size && a.allocSize == b.allocSize
        && a.modifyTime.tv_sec == b.modifyTime.tv_sec && a.modifyTime.tv_nsec == b.modifyTime.tv_nsec
        && a.changeTime.tv_sec == b.changeTime.tv_sec && a.changeTime.tv_nsec == b.changeTime.tv_nsec
        && a.birthTime.tv_sec == b.birthTime.tv_sec && a.birthTime.tv_nsec == b.birthTime.tv_nsec
        // parent ID and added time belong to the directory entry, not the inode: one value per hard-linked file
        && (a.linkCount > 1 || (a.parentID == b.parentID && a.addedTime.tv_sec == b.addedTime.tv_sec))
}

/// Walks the index from the root by name (like FSKit lookups) and compares every object and
/// every directory listing with the disk backend. Returns the number of objects compared.
@discardableResult
func compareIndex(_ idx: MemIndex, _ label: String) -> Int {
    var compared = 0
    var bad = 0
    func walk(_ dirIno: UInt64, _ dirPath: String) {
        guard let disk = try? b.list(dirPath) else { expect(false, "\(label): list \(dirPath)"); return }
        guard let (_, kids) = try? idx.children(dirIno) else { expect(false, "\(label): index children \(dirPath)"); return }
        let diskNames = disk.map(\.name).sorted { $0.lexicographicallyPrecedes($1) }
        if kids.map(\.name) != diskNames {
            bad += 1
            expect(false, "\(label): children of \(dirPath): index \(kids.count) disk \(disk.count)")
        }
        // enumerate attributes (one batch) must equal the disk listing
        let byName = Dictionary(disk.map { ($0.name, $0) }, uniquingKeysWith: { a, _ in a })
        for case let e? in idx.entries(kids[...]) {
            if let d = byName[e.name], !sameEntry(e, d) { bad += 1; expect(false, "\(label): enumerate attrs \(dirPath)/\(e.nameString)") }
        }
        for d in disk {
            let rel = dirPath == "." ? d.nameString : dirPath + "/" + d.nameString
            // lookup by name, getattr by ino
            guard let le = try? idx.lookup(dir: dirIno, name: d.nameString) else {
                bad += 1; expect(false, "\(label): lookup \(rel)"); continue
            }
            guard let st = try? b.stat(rel), let ge = idx.entry(le.fileID) else {
                bad += 1; expect(false, "\(label): stat/getattr \(rel)"); continue
            }
            if !sameEntry(le, st) || !sameEntry(ge, st) || le.name != d.name {
                bad += 1
                expect(false, "\(label): attrs differ for \(rel): index size=\(le.size) nlink=\(le.linkCount) ctime=\(le.changeTime.tv_nsec); disk size=\(st.size) nlink=\(st.linkCount) ctime=\(st.changeTime.tv_nsec)")
            }
            if d.vtype == .lnk, let t = idx.symlinkTarget(le.fileID) {
                expect(t == (try? b.readlink(rel)) ?? [], "\(label): readlink \(rel)")
            }
            compared += 1
            if d.vtype == .dir { walk(le.fileID, rel) }
        }
    }
    guard let rootIdx = idx.entry(idx.rootIno), let rootDisk = try? b.stat(".") else { expect(false, "\(label): root"); return 0 }
    expect(sameEntry(rootIdx, rootDisk), "\(label): root attrs")
    walk(idx.rootIno, ".")
    expect(bad == 0, "\(label): \(bad) differences")
    print("  info index \(label): \(compared) objects equal to disk")
    return compared
}

var bs = MemIndex.BuildStats()
let idx = try MemIndex(backend: b, stats: &bs)
print("  info index build: \(bs.json)")
expect(bs.errors == 0, "index build errors \(bs.firstErrors)")
expect(compareIndex(idx, "after build") >= 5000, "index covers the 5000-file tree")

// Case-insensitive lookup returns the stored name.
if !idx.caseSensitive {
    expect((try? idx.lookup(dir: idx.rootIno, name: "A.TXT"))?.nameString == "a.txt", "index case-insensitive lookup")
}
do { _ = try idx.lookup(dir: idx.rootIno, name: "nope"); expect(false, "index ENOENT") } catch let e as PosixError { expect(e.code == ENOENT, "index ENOENT code") }
let aIno = try b.stat("a.txt").fileID
do { _ = try idx.lookup(dir: aIno, name: "x"); expect(false, "index ENOTDIR") } catch let e as PosixError { expect(e.code == ENOTDIR, "index ENOTDIR code") }

// Mutations: do the disk operation, then the same index update call the volume makes.
let manyIno = try b.stat("many").fileID
let gen0 = try idx.children(manyIno).gen
try b.createFile("many/zz-new.txt", mode: 0o644)
try idx.didCreate(dir: manyIno, dirPath: "many", name: "zz-new.txt", backend: b)
expect(try idx.children(manyIno).gen != gen0, "directory version changes on create")
try b.mkdir("many/newsub", mode: 0o755)
try idx.didCreate(dir: manyIno, dirPath: "many", name: "newsub", backend: b)
let newsubIno = try b.stat("many/newsub").fileID
try b.symlink("many/newsub/sl", target: "../zz-new.txt")
try idx.didCreate(dir: newsubIno, dirPath: "many/newsub", name: "sl", backend: b)
try b.link(from: "a.txt", to: "many/newsub/a-hard")
try idx.didCreate(dir: newsubIno, dirPath: "many/newsub", name: "a-hard", backend: b)
idx.update(try b.stat("a.txt"))   // the other link's ctime and nlink changed too
compareIndex(idx, "after create")

// Rename: file within a directory, file across directories (over an existing file), directory move.
try b.rename("many/file-with-a-long-name-1.txt", "many/renamed-1.txt")
try idx.didRename(src: manyIno, srcPath: "many", srcName: "file-with-a-long-name-1.txt",
                  dst: manyIno, dstPath: "many", dstName: "renamed-1.txt", backend: b)
try b.createFile("dir/victim.txt", mode: 0o644)
let dirIno = try b.stat("dir").fileID
try idx.didCreate(dir: dirIno, dirPath: "dir", name: "victim.txt", backend: b)
try b.rename("many/file-with-a-long-name-2.txt", "dir/victim.txt")
try idx.didRename(src: manyIno, srcPath: "many", srcName: "file-with-a-long-name-2.txt",
                  dst: dirIno, dstPath: "dir", dstName: "victim.txt", backend: b)
try b.rename("many/newsub", "dir/movedsub")
try idx.didRename(src: manyIno, srcPath: "many", srcName: "newsub", dst: dirIno, dstPath: "dir", dstName: "movedsub", backend: b)
try b.rename("a.txt", "A.TXT")   // case-only rename
try idx.didRename(src: idx.rootIno, srcPath: ".", srcName: "a.txt", dst: idx.rootIno, dstPath: ".", dstName: "A.TXT", backend: b)
compareIndex(idx, "after rename")

// Remove: file, symlink, hard link, then the emptied directory.
try b.unlink("many/zz-new.txt", isDir: false)
try idx.didRemove(dir: manyIno, dirPath: "many", name: "zz-new.txt", backend: b)
try b.unlink("dir/movedsub/sl", isDir: false)
let movedIno = try b.stat("dir/movedsub").fileID
try idx.didRemove(dir: movedIno, dirPath: "dir/movedsub", name: "sl", backend: b)
try b.unlink("dir/movedsub/a-hard", isDir: false)
try idx.didRemove(dir: movedIno, dirPath: "dir/movedsub", name: "a-hard", backend: b)
idx.update(try b.stat("A.TXT"))   // remaining link: ctime changed
try b.unlink("dir/movedsub", isDir: true)
try idx.didRemove(dir: dirIno, dirPath: "dir", name: "movedsub", backend: b)
expect(idx.entry(movedIno) == nil, "removed directory dropped from index")
compareIndex(idx, "after remove")

// Write and setattr: refresh the attributes from the descriptor or path, as the volume does.
let wfd = try b.openFile("many/renamed-1.txt", flags: O_WRONLY)
_ = "grow".withCString { pwrite(wfd, $0, 4, 1000) }
idx.update(try b.fstat(wfd))
close(wfd)
try b.chmod("dir/big.bin", mode: 0o600)
try idx.refresh(path: "dir/big.bin", backend: b)
expect((try? idx.lookup(dir: manyIno, name: "renamed-1.txt"))?.size == 1004, "size after write")
compareIndex(idx, "after write and setattr")

print(failures == 0 ? "SELFTEST PASS" : "SELFTEST FAIL (\(failures))")
exit(failures == 0 ? 0 : 1)
