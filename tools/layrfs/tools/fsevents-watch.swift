// fsevents-watch <watch-dir> <file-to-write> <timeout-seconds>
//
// Starts an FSEventStream on <watch-dir>, then writes <file-to-write> itself, and waits for an
// event whose path ends with the file's name.
// Prints "EVENT_OK <ms> <event-path>" (exit 0) or "EVENT_TIMEOUT" (exit 1).
// Writing from inside the tool avoids a race between watcher start and the write.

import CoreServices
import Foundation

let args = CommandLine.arguments
guard args.count == 4, let timeout = Double(args[3]) else {
    FileHandle.standardError.write("usage: fsevents-watch <watch-dir> <file-to-write> <timeout-seconds>\n".data(using: .utf8)!)
    exit(2)
}
let watchDir = (args[1] as NSString).standardizingPath
let target = args[2]
let targetName = (target as NSString).lastPathComponent
var writeTime: CFAbsoluteTime = 0

let callback: FSEventStreamCallback = { _, _, count, pathsPtr, _, _ in
    let paths = unsafeBitCast(pathsPtr, to: NSArray.self) as? [String] ?? []
    for p in paths.prefix(count) where (p as NSString).lastPathComponent == targetName {
        let ms = (CFAbsoluteTimeGetCurrent() - writeTime) * 1000
        print(String(format: "EVENT_OK %.1f %@", ms, p))
        exit(0)
    }
}

var ctx = FSEventStreamContext(version: 0, info: nil, retain: nil, release: nil, copyDescription: nil)
let flags = UInt32(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagUseCFTypes)
guard let stream = FSEventStreamCreate(nil, callback, &ctx, [watchDir] as CFArray,
                                       FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 0.05, flags)
else {
    print("EVENT_ERROR stream create failed")
    exit(3)
}
FSEventStreamSetDispatchQueue(stream, DispatchQueue.main)
guard FSEventStreamStart(stream) else {
    print("EVENT_ERROR stream start failed")
    exit(3)
}

DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
    writeTime = CFAbsoluteTimeGetCurrent()
    do {
        try "fsevents probe \(Date())\n".write(toFile: target, atomically: false, encoding: .utf8)
    } catch {
        print("EVENT_ERROR write failed: \(error)")
        exit(3)
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + timeout) {
        print("EVENT_TIMEOUT")
        exit(1)
    }
}
dispatchMain()
