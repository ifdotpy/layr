// FSUnaryFileSystem: one FSPathURLResource becomes one passthrough volume.

import CryptoKit
import Foundation
import FSKit

final class LayrFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    private var volume: LayrVolume?

    override init() {
        super.init()
        Stats.shared.installSignalHandlers()
        fsLog.notice("layrfs module loaded pid=\(getpid())")
    }

    /// A stable UUID for a path, so the same directory gets the same container and volume ID.
    private static func uuid(for path: String) -> UUID {
        let d = Array(Insecure.MD5.hash(data: Data(path.utf8)))
        return UUID(uuid: (d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
                           d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]))
    }

    func probeResource(resource: FSResource, replyHandler reply: @escaping @Sendable (FSProbeResult?, (any Error)?) -> Void) {
        guard let r = resource as? FSPathURLResource else {
            reply(FSProbeResult.notRecognized, nil)
            return
        }
        let path = r.url.path
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir), isDir.boolValue else {
            fsLog.error("layrfs probe: not a directory: \(path, privacy: .public)")
            reply(FSProbeResult.notRecognized, nil)
            return
        }
        let name = r.url.lastPathComponent.isEmpty ? "layrfs" : r.url.lastPathComponent
        reply(FSProbeResult.usable(name: name, containerID: FSContainerIdentifier(uuid: Self.uuid(for: path))), nil)
    }

    func loadResource(resource: FSResource, options: FSTaskOptions,
                      replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void)
    {
        guard let r = resource as? FSPathURLResource else {
            reply(nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        let opts = options.taskOptions
        Stats.shared.setNote("load_task_options", opts)
        let config = Config.load(taskOptions: opts)
        let readOnly = !r.isWritable || opts.contains("--rdonly") || opts.contains("-r")
        do {
            let v = try LayrVolume(resource: r, config: config, readOnly: readOnly, volumeID: Self.uuid(for: r.url.path))
            volume = v
            containerStatus = .ready
            reply(v, nil)
        } catch let e as PosixError {
            fsLog.error("layrfs load failed: \(e.description, privacy: .public)")
            reply(nil, fs_errorForPOSIXError(e.code))
        } catch {
            reply(nil, error)
        }
    }

    func unloadResource(resource: FSResource, options: FSTaskOptions,
                        replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    {
        Stats.shared.dump(reason: "unloadResource")
        volume = nil
        reply(nil)
    }

    func didFinishLoading() {
        fsLog.notice("layrfs didFinishLoading")
    }
}
