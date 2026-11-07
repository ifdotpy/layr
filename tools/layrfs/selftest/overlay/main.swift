// Self-test of the overlay merge logic (Overlay.swift), without a mount.
// Each step runs on the overlay and on a plain mirror folder; the merged view must equal the mirror.
// Build and run: selftest/run.sh (it runs this test after the backend test).

import Darwin
import Foundation

var failures = 0
func expect(_ cond: @autoclosure () throws -> Bool, _ msg: String, line: Int = #line) {
    if !((try? cond()) ?? false) { failures += 1; print("FAIL line \(line): \(msg)") }
}

func errno(of body: () throws -> Void) -> Int32 {
    do { try body(); return 0 } catch let e as PosixError { return e.code } catch { return -1 }
}

let fm = FileManager.default
let tmpBase = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : NSTemporaryDirectory()
let base = (tmpBase as NSString).appendingPathComponent("layrfs-overlay-selftest-\(getpid())")
let lowerDir = base + "/lower", upperDir = base + "/upper", mirror = base + "/mirror"
try fm.createDirectory(atPath: lowerDir + "/dir/sub", withIntermediateDirectories: true)
try fm.createDirectory(atPath: lowerDir + "/emptydir", withIntermediateDirectories: true)
try fm.createDirectory(atPath: lowerDir + "/deep/a/b", withIntermediateDirectories: true)
try fm.createDirectory(atPath: upperDir, withIntermediateDirectories: true)
func put(_ rel: String, _ text: String, root: String = lowerDir) {
    try! Data(text.utf8).write(to: URL(fileURLWithPath: root + "/" + rel))
}
put("a.txt", "hello")
put("dir/b.txt", "bee")
put("dir/sub/c.txt", "sea")
put("deep/a/b/d.txt", "dee")
put("Mixed Case.txt", "mixed")
try fm.createSymbolicLink(atPath: lowerDir + "/link", withDestinationPath: "a.txt")
chmod(lowerDir + "/dir/sub", 0o750)
// The mirror starts as a plain copy of the lower tree.
let cp = Process()
cp.executableURL = URL(fileURLWithPath: "/bin/cp")
cp.arguments = ["-Rp", lowerDir, mirror]
try cp.run(); cp.waitUntilExit()
// Seal the lower tree as Layr seals a state: the immutable flag on every entry.
func walkPaths(_ root: String) -> [String] {
    var out: [String] = []
    if let en = fm.enumerator(atPath: root) { while let p = en.nextObject() as? String { out.append(p) } }
    return out
}
func setFlags(_ root: String, _ on: Bool) {
    for p in walkPaths(root).sorted().reversed() + [""] {
        let abs = p.isEmpty ? root : root + "/" + p
        var s = Darwin.stat()
        lstat(abs, &s)
        _ = lchflags(abs, on ? (s.st_flags | UInt32(UF_IMMUTABLE)) : (s.st_flags & ~UInt32(UF_IMMUTABLE)))
    }
}
setFlags(lowerDir, true)
defer { setFlags(lowerDir, false); try? fm.removeItem(atPath: base) }

var ov = try Overlay(lowerPath: lowerDir, upperPath: upperDir)
var rs = Darwin.stat()
lstat(lowerDir, &rs)
expect(ov.rootIno == rs.st_ino, "root ID is the lower root inode")
expect((try ov.stat(".")).fileID == rs.st_ino, "stat of root")
expect((try ov.stat("a.txt")).flags & lockFlags == 0, "lower flags are masked")

func readAll(_ fd: Int32) -> String {
    var buf = [UInt8](repeating: 0, count: 65536)
    let n = pread(fd, &buf, buf.count, 0)
    return String(decoding: buf[0..<max(n, 0)], as: UTF8.self)
}

func lowerIno(_ rel: String) -> UInt64 { var s = Darwin.stat(); lstat(lowerDir + "/" + rel, &s); return s.st_ino }

/// Writes `text` to the merged file at `rel` and to the mirror.
func write(_ rel: String, _ text: String) throws {
    let fd = try ov.openFile(rel, flags: O_RDWR | O_TRUNC, mode: 0)
    _ = text.withCString { pwrite(fd, $0, strlen($0), 0) }
    close(fd)
    put(rel, text, root: mirror)
}

/// Compares the merged view with the mirror: names, types, modes, sizes, content, link targets.
func compareWithMirror(_ label: String) {
    var mergedPaths: [String: Entry] = [:]
    var queue = ["."]
    var i = 0
    while i < queue.count {
        let d = queue[i]
        i += 1
        guard let list = try? ov.list(d) else { expect(false, "\(label): list \(d)"); continue }
        for e in list {
            let p = RelPath.join(d, e.nameString)
            mergedPaths[p] = e
            if e.vtype == .dir { queue.append(p) }
        }
    }
    let mirrorPaths = Set(walkPaths(mirror))
    expect(Set(mergedPaths.keys) == mirrorPaths,
           "\(label): paths differ: only merged \(Set(mergedPaths.keys).subtracting(mirrorPaths).sorted()), only mirror \(mirrorPaths.subtracting(mergedPaths.keys).sorted())")
    for (p, e) in mergedPaths where mirrorPaths.contains(p) {
        var s = Darwin.stat()
        lstat(mirror + "/" + p, &s)
        expect(e.fullMode == UInt32(s.st_mode), "\(label): \(p) mode \(String(e.fullMode, radix: 8)) != \(String(s.st_mode, radix: 8))")
        guard let st = try? ov.stat(p) else { expect(false, "\(label): stat \(p)"); continue }
        expect(st.fileID == e.fileID, "\(label): \(p) stat and list IDs differ")
        switch e.vtype {
        case .reg:
            expect(e.size == UInt64(s.st_size), "\(label): \(p) size")
            if let fd = try? ov.openFile(p, flags: O_RDONLY, mode: 0) {
                let want = (try? String(contentsOfFile: mirror + "/" + p, encoding: .utf8)) ?? "?"
                expect(readAll(fd) == want, "\(label): \(p) content")
                close(fd)
            } else { expect(false, "\(label): open \(p)") }
        case .lnk:
            let want = (try? fm.destinationOfSymbolicLink(atPath: mirror + "/" + p)) ?? "?"
            expect((try? ov.readlink(p)).map { String(decoding: $0, as: UTF8.self) } == want, "\(label): \(p) link target")
        default: break
        }
    }
    // IDs are unique in the merged view, except for hard links.
    let ids = mergedPaths.values.filter { $0.linkCount <= 1 }.map(\.fileID)
    expect(Set(ids).count == ids.count, "\(label): IDs are not unique")
}

compareWithMirror("start")

// Write to a lower file: copy-up keeps the lower inode as the ID, the lower file stays.
let aIno = lowerIno("a.txt")
try write("a.txt", "HELLO WORLD")
expect((try ov.stat("a.txt")).fileID == aIno, "copied-up file keeps the lower ID")
expect(ov.inUpper("a.txt"), "a.txt is in the upper folder")
expect((try? String(contentsOfFile: lowerDir + "/a.txt", encoding: .utf8)) == "hello", "lower file unchanged")
var us = Darwin.stat()
lstat(upperDir + "/a.txt", &us)
expect(us.st_flags & UInt32(UF_IMMUTABLE) == 0, "upper copy is not immutable")
compareWithMirror("write lower file")

// New file in a lower folder: the folder is copied up with the lower ID and mode.
let dirIno = lowerIno("dir/sub")
try ov.createFile("dir/sub/new.txt", mode: 0o644)
put("dir/sub/new.txt", "", root: mirror)
try write("dir/sub/new.txt", "new")
expect((try ov.stat("dir/sub")).fileID == dirIno, "copied-up folder keeps the lower ID")
expect((try ov.stat("dir/sub")).perm == 0o750, "copied-up folder keeps the lower mode")
expect(errno { try ov.createFile("dir/sub/c.txt", mode: 0o644) } == EEXIST, "create over a lower file fails")
expect(errno { try ov.createFile("nodir/x", mode: 0o644) } == ENOENT, "create in a missing folder fails")
compareWithMirror("create")

// Delete a lower file: a whiteout.
try ov.unlink("dir/b.txt", isDir: false)
try fm.removeItem(atPath: mirror + "/dir/b.txt")
expect(errno { _ = try ov.stat("dir/b.txt") } == ENOENT, "deleted file is gone")
expect(ov.whiteouts == ["dir/b.txt"], "whiteout recorded: \(ov.whiteouts)")
compareWithMirror("delete lower file")

// Rename a lower file: a clone at the new path with the lower ID, and a whiteout.
let cIno = lowerIno("dir/sub/c.txt")
try ov.rename("dir/sub/c.txt", "c2.txt")
try fm.moveItem(atPath: mirror + "/dir/sub/c.txt", toPath: mirror + "/c2.txt")
expect((try ov.stat("c2.txt")).fileID == cIno, "renamed lower file keeps its ID")
compareWithMirror("rename lower file")

// A lower folder cannot move (EXDEV); an empty one can go.
expect(errno { try ov.rename("deep/a", "deep2") } == EXDEV, "rename of a lower folder is EXDEV")
expect(errno { try ov.unlink("dir", isDir: true) } == ENOTEMPTY, "rmdir of a full folder fails")
try ov.unlink("emptydir", isDir: true)
try fm.removeItem(atPath: mirror + "/emptydir")
compareWithMirror("rmdir")

// Remove a whole lower subtree, then make the folder again: it is opaque.
try ov.unlink("deep/a/b/d.txt", isDir: false)
try ov.unlink("deep/a/b", isDir: true)
try ov.unlink("deep/a", isDir: true)
try fm.removeItem(atPath: mirror + "/deep/a")
expect(ov.whiteouts.contains("deep/a") && !ov.whiteouts.contains("deep/a/b"), "whiteouts below a removed folder go: \(ov.whiteouts)")
try ov.mkdir("deep/a", mode: 0o755)
try fm.createDirectory(atPath: mirror + "/deep/a", withIntermediateDirectories: false)
expect((try ov.list("deep/a")).isEmpty, "a folder made again over a removed one is empty")
compareWithMirror("opaque folder")

// Upper entries: rename a new file, rename a new folder with content, a link and a hard link.
try ov.mkdir("newdir", mode: 0o755)
try ov.createFile("newdir/x.txt", mode: 0o600)
try fm.createDirectory(atPath: mirror + "/newdir", withIntermediateDirectories: false)
put("newdir/x.txt", "", root: mirror)
chmod(mirror + "/newdir/x.txt", 0o600)
try write("newdir/x.txt", "x")
try ov.rename("newdir", "dir/moved")
try fm.moveItem(atPath: mirror + "/newdir", toPath: mirror + "/dir/moved")
try ov.rename("dir/sub/new.txt", "dir/moved/new.txt")
try fm.moveItem(atPath: mirror + "/dir/sub/new.txt", toPath: mirror + "/dir/moved/new.txt")
try ov.symlink("dir/l2", target: "../a.txt")
try fm.createSymbolicLink(atPath: mirror + "/dir/l2", withDestinationPath: "../a.txt")
try ov.link(from: "Mixed Case.txt", to: "hard.txt")
_ = Darwin.link(mirror + "/Mixed Case.txt", mirror + "/hard.txt")
compareWithMirror("upper changes")

// Rename over an existing lower file, and rename of a copied-up file (the lower name needs a whiteout).
try ov.rename("c2.txt", "link")
try fm.removeItem(atPath: mirror + "/link")
try fm.moveItem(atPath: mirror + "/c2.txt", toPath: mirror + "/link")
try write("a.txt", "again")
try ov.rename("a.txt", "a-moved.txt")
try fm.moveItem(atPath: mirror + "/a.txt", toPath: mirror + "/a-moved.txt")
expect(errno { _ = try ov.stat("a.txt") } == ENOENT, "the lower file does not come back after a rename of its copy")
compareWithMirror("rename over")

// Case-insensitive lookups find the stored name.
if !ov.keys.caseSensitive {
    expect((try? ov.stat("MIXED CASE.TXT"))?.nameString == "Mixed Case.txt", "case-insensitive lookup returns the stored name")
}

// Reload from disk: the same merged view, the same whiteouts in the layer attribute.
let before = ov.whiteouts
ov = try Overlay(lowerPath: lowerDir, upperPath: upperDir)
expect(ov.whiteouts == before, "whiteouts survive a reload")
compareWithMirror("reload")
let raw = try Backend(rootPath: upperDir).getXattr(".", name: "com.layr.removed")
expect((try JSONSerialization.jsonObject(with: raw) as? [String]) == before, "com.layr.removed is a sorted JSON array")
expect((try? ov.getXattr(".", name: "com.layr.removed")) == nil, "the layer attribute is hidden")
expect(!(try ov.listXattrs(".")).contains(Array("com.layr.removed".utf8)), "the layer attribute is not listed")

if failures == 0 { print("OVERLAY SELFTEST PASS") } else { print("OVERLAY SELFTEST FAIL: \(failures)") }
exit(failures == 0 ? 0 : 1)
