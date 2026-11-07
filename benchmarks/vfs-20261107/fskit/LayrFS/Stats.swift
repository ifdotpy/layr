// Operation counters, latency, the open ring, and the JSON dump.
// No FSKit dependency, so selftest/ can compile it.

import Darwin
import Foundation
import os

let fsLog = Logger(subsystem: "dev.layr.fs", category: "fs")

enum Op: Int, CaseIterable {
    case activate, deactivate, mount, unmount, synchronize, statfs
    case lookup, reclaim, getattr, setattr
    case create, mkdir, symlink, link, readlink, remove, rename, enumerate
    case open, close, cacheUpgrade, read, write
    case getxattr, setxattr, listxattr

    var name: String { String(describing: self) }
}

@inline(__always) func nowNs() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }

struct OpCounter {
    var count: UInt64 = 0
    var errors: UInt64 = 0
    var enoent: UInt64 = 0
    var totalNs: UInt64 = 0
    var maxNs: UInt64 = 0
    var bytes: UInt64 = 0
    /// Bucket i counts calls with latency in [2^i, 2^(i+1)) microseconds; bucket 0 also holds < 1 µs.
    var histogram = [UInt64](repeating: 0, count: 24)
}

struct OpenRecord {
    var t: Double              // seconds since stats start
    var event: String          // open, close, cacheOpen, cacheClose, cacheUpgrade
    var path: String
    var modes: String
    var cacheMode: String
    var granted: String
    var ruid: Int, euid: Int, rgid: Int, egid: Int
    var error: Int32
}

final class Stats: @unchecked Sendable {
    static let shared = Stats()

    private let lock = NSLock()
    private var counters = [OpCounter](repeating: OpCounter(), count: Op.allCases.count)
    private var ring: [OpenRecord] = []
    private var ringNext = 0
    private var ringTotal: UInt64 = 0
    private let ringCapacity = 8192
    private var startNs = nowNs()
    private var startDate = Date()
    private var tempOpens: UInt64 = 0
    private var notes: [String: Any] = [:]
    private var signalSources: [DispatchSourceSignal] = []
    private var dumpSeq = 0

    /// Extra JSON fields set by the volume (backing path, config, caller identity probe).
    func setNote(_ key: String, _ value: Any) {
        lock.lock(); notes[key] = value; lock.unlock()
    }

    func record(_ op: Op, ns: UInt64, error: Int32 = 0, bytes: Int = 0) {
        let us = ns / 1000
        var bucket = 0
        if us > 0 { bucket = min(63 - us.leadingZeroBitCount, 23) }
        lock.lock()
        var c = counters[op.rawValue]
        c.count += 1
        c.totalNs += ns
        if ns > c.maxNs { c.maxNs = ns }
        if error != 0 { c.errors += 1; if error == ENOENT { c.enoent += 1 } }
        c.bytes += UInt64(max(bytes, 0))
        c.histogram[bucket] += 1
        counters[op.rawValue] = c
        lock.unlock()
    }

    func addBytes(_ op: Op, _ n: Int) {
        lock.lock(); counters[op.rawValue].bytes += UInt64(max(n, 0)); lock.unlock()
    }

    func countTempOpen() { lock.lock(); tempOpens += 1; lock.unlock() }

    func recordOpen(_ r: OpenRecord) {
        var r = r
        lock.lock()
        r.t = Double(nowNs() - startNs) / 1e9
        if ring.count < ringCapacity { ring.append(r) } else { ring[ringNext] = r }
        ringNext = (ringNext + 1) % ringCapacity
        ringTotal += 1
        lock.unlock()
    }

    /// Clears counters and the ring (SIGUSR2). Notes stay.
    func reset() {
        lock.lock()
        counters = [OpCounter](repeating: OpCounter(), count: Op.allCases.count)
        ring = []; ringNext = 0; ringTotal = 0; tempOpens = 0
        startNs = nowNs(); startDate = Date()
        lock.unlock()
        fsLog.notice("layrfs stats reset")
    }

    func snapshot(reason: String) -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        var ops: [String: Any] = [:]
        for op in Op.allCases {
            let c = counters[op.rawValue]
            if c.count == 0 { continue }
            var hist: [String: UInt64] = [:]
            for (i, n) in c.histogram.enumerated() where n > 0 { hist["lt\(1 << (i + 1))us"] = n }
            ops[op.name] = [
                "count": c.count,
                "errors": c.errors,
                "enoent": c.enoent,
                "total_ms": Double(c.totalNs) / 1e6,
                "avg_us": Double(c.totalNs) / Double(c.count) / 1e3,
                "max_us": Double(c.maxNs) / 1e3,
                "bytes": c.bytes,
                "histogram": hist,
            ]
        }
        // Ring in time order.
        var ordered: [OpenRecord] = []
        if ring.count < ringCapacity { ordered = ring } else { ordered = Array(ring[ringNext...] + ring[..<ringNext]) }
        let opens: [[String: Any]] = ordered.map {
            ["t": $0.t, "event": $0.event, "path": $0.path, "modes": $0.modes, "cacheMode": $0.cacheMode,
             "granted": $0.granted, "ruid": $0.ruid, "euid": $0.euid, "rgid": $0.rgid, "egid": $0.egid,
             "error": $0.error]
        }
        var out: [String: Any] = [
            "reason": reason,
            "pid": Int(getpid()),
            "since": ISO8601DateFormatter().string(from: startDate),
            "elapsed_s": Double(nowNs() - startNs) / 1e9,
            "latency_note": "module-side time from callback entry to reply call; XPC and kernel time are not included",
            "ops": ops,
            "temp_opens_for_io": tempOpens,
            "open_ring_total": ringTotal,
            "open_ring_capacity": ringCapacity,
            "open_ring": opens,
        ]
        for (k, v) in notes { out[k] = v }
        return out
    }

    /// Writes the JSON to NSTemporaryDirectory() and logs a summary with os_log.
    @discardableResult
    func dump(reason: String) -> String? {
        let snap = snapshot(reason: reason)
        lock.lock(); dumpSeq += 1; let seq = dumpSeq; lock.unlock()
        let dir = NSTemporaryDirectory()
        let stamp = Int(Date().timeIntervalSince1970)
        let path = (dir as NSString).appendingPathComponent("layrfs-stats-\(getpid())-\(stamp)-\(seq).json")
        let latest = (dir as NSString).appendingPathComponent("layrfs-stats-latest.json")
        var written: String?
        do {
            let data = try JSONSerialization.data(withJSONObject: snap, options: [.prettyPrinted, .sortedKeys])
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
            try data.write(to: URL(fileURLWithPath: latest), options: .atomic)
            written = path
        } catch {
            fsLog.error("layrfs stats write failed: \(String(describing: error), privacy: .public)")
        }
        fsLog.notice("layrfs stats dump reason=\(reason, privacy: .public) file=\(written ?? "-", privacy: .public)")
        if let ops = snap["ops"] as? [String: [String: Any]] {
            for (name, c) in ops.sorted(by: { $0.key < $1.key }) {
                let line = "op=\(name) count=\(c["count"]!) errors=\(c["errors"]!) enoent=\(c["enoent"]!) total_ms=\(c["total_ms"]!) avg_us=\(c["avg_us"]!) max_us=\(c["max_us"]!) bytes=\(c["bytes"]!)"
                fsLog.notice("layrfs stats \(line, privacy: .public)")
            }
        }
        return written
    }

    /// SIGUSR1 dumps, SIGUSR2 resets. Call once per process.
    func installSignalHandlers() {
        lock.lock()
        defer { lock.unlock() }
        if !signalSources.isEmpty { return }
        let q = DispatchQueue(label: "dev.layr.fs.signals")
        let actions: [(Int32, () -> Void)] = [
            (SIGUSR1, { Stats.shared.dump(reason: "SIGUSR1") }),
            (SIGUSR2, { Stats.shared.reset(); Stats.shared.dump(reason: "SIGUSR2-reset") }),
        ]
        for (sig, action) in actions {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(signal: sig, queue: q)
            src.setEventHandler { action() }
            src.resume()
            signalSources.append(src)
        }
    }
}

/// Runtime settings. Read from `NSHomeDirectory()/layrfs.conf` (inside the sandbox this is
/// ~/Library/Containers/dev.layr.fshost.fs/Data/layrfs.conf) and from mount task options.
struct Config {
    enum DataCache: String {
        case off            // do not use FSVolume.DataCacheHandler; FSKit uses OpenCloseHandler
        case nocache        // DataCacheHandler, always grant noCache
        case read           // grant readCache at most
        case writethrough   // grant writeThrough for read-write opens
        case writeback      // grant writeBack for read-write opens (most caching)
    }

    var dataCache: DataCache = .writeback
    var logOpens = true
    /// false: datacache=off and FSKit skips open/close calls (isOpenCloseInhibited)
    var openClose = true
    /// memory: serve lookup, getattr, enumerate, readlink and statfs from an index built at load time
    var indexMemory = false
    /// true: enumerateDirectory replies at once with no entries (XPC floor of one enumerate call)
    var nullEnumerateProbe = false
    var source: [String] = []

    static func load(taskOptions: [String]) -> Config {
        var c = Config()
        var pairs: [(String, String)] = []
        let confPath = (NSHomeDirectory() as NSString).appendingPathComponent("layrfs.conf")
        if let text = try? String(contentsOfFile: confPath, encoding: .utf8) {
            c.source.append(confPath)
            for line in text.split(whereSeparator: \.isNewline) {
                let l = line.trimmingCharacters(in: .whitespaces)
                if l.isEmpty || l.hasPrefix("#") { continue }
                let kv = l.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
                if kv.count == 2 { pairs.append((kv[0], kv[1])) }
            }
        }
        // Task options can carry "layrfs.datacache=readonly" style tokens (for example from mount -o).
        for opt in taskOptions {
            for tok in opt.split(separator: ",") {
                let kv = tok.split(separator: "=", maxSplits: 1).map(String.init)
                if kv.count == 2, kv[0].hasPrefix("layrfs.") {
                    pairs.append((String(kv[0].dropFirst("layrfs.".count)), kv[1]))
                    c.source.append("taskOption:\(tok)")
                }
            }
        }
        for (k, v) in pairs {
            switch k {
            case "datacache": if let d = DataCache(rawValue: v) { c.dataCache = d }
            case "logopens": c.logOpens = (v == "1" || v == "true" || v == "yes")
            case "openclose": c.openClose = !(v == "0" || v == "false" || v == "off" || v == "no")
            case "index": c.indexMemory = (v == "memory")
            case "null_enumerate_probe": c.nullEnumerateProbe = (v == "1" || v == "true" || v == "yes" || v == "on")
            default: break
            }
        }
        if !c.openClose { c.dataCache = .off }
        return c
    }

    var asJSON: [String: Any] {
        ["datacache": dataCache.rawValue, "logopens": logOpens, "openclose": openClose,
         "index": indexMemory ? "memory" : "disk", "null_enumerate_probe": nullEnumerateProbe, "source": source]
    }
}
