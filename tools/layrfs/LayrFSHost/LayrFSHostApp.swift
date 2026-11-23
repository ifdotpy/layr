// The host app. It only carries the LayrFS extension and shows its state.
//
// Without arguments it opens a window; when the extension is off, it also opens the settings
// pane where a person turns it on. For layr (no window):
//   Layr --status         prints "enabled", "disabled" or "missing" and exits
//   Layr --open-settings  opens the settings pane and exits

import FSKit
import SwiftUI

let moduleID = "dev.layr.fshost.fs"

enum ModuleState: String {
    case enabled, disabled, missing
}

func moduleState() async throws -> ModuleState {
    let mine = try await FSClient.shared.installedExtensions.filter { $0.bundleIdentifier == moduleID }
    if mine.isEmpty { return .missing }
    return mine.contains { $0.isEnabled } ? .enabled : .disabled
}

@main
enum Entry {
    static func main() {
        let args = CommandLine.arguments.dropFirst()
        if args.contains("--status") {
            Task {
                do {
                    print(try await moduleState().rawValue)
                    exit(0)
                } catch {
                    FileHandle.standardError.write(Data("FSClient: \(error.localizedDescription)\n".utf8))
                    exit(1)
                }
            }
            dispatchMain()
        }
        if args.contains("--open-settings") {
            Task { @MainActor in
                _ = FSClient.shared.openFileSystemExtensionsSettings()
                exit(0)
            }
            dispatchMain()
        }
        LayrFSHostApp.main()
    }
}

struct LayrFSHostApp: App {
    var body: some Scene {
        WindowGroup("LayrFS") { ContentView() }
    }
}

struct ContentView: View {
    @State private var status = "Checking the LayrFS extension..."
    @State private var opened = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("LayrFS").font(.headline)
            Text("The file system of fast lines (layr line new --fast).")
            Text(status).font(.system(.body, design: .monospaced)).textSelection(.enabled)
            HStack {
                Button("Refresh") { Task { await refresh() } }
                Button("Open File System Extensions Settings") {
                    _ = FSClient.shared.openFileSystemExtensionsSettings()
                }
            }
        }
        .padding(20)
        .frame(minWidth: 520, minHeight: 180)
        .task { await refresh() }
    }

    private func refresh() async {
        do {
            switch try await moduleState() {
            case .enabled:
                status = "LayrFS is on. You can close this window."
            case .disabled:
                status = "LayrFS is off. Turn it on in File System Extensions."
                if !opened {
                    opened = true
                    _ = FSClient.shared.openFileSystemExtensionsSettings()
                }
            case .missing:
                status = "\(moduleID) is not installed. Keep this app in /Applications and open it again."
            }
        } catch {
            status = "FSClient error: \(error.localizedDescription)"
        }
    }
}
