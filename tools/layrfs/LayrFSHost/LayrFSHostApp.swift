// Minimal host app. It only carries the LayrFS extension and shows its state.

import FSKit
import SwiftUI

@main
struct LayrFSHostApp: App {
    var body: some Scene {
        WindowGroup("LayrFS Host") { ContentView() }
    }
}

struct ContentView: View {
    @State private var status = "Checking installed FSKit modules..."

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("LayrFS passthrough prototype").font(.headline)
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
            let mods = try await FSClient.shared.installedExtensions
            let mine = mods.filter { $0.bundleIdentifier == "dev.layr.fshost.fs" }
            status = mine.isEmpty
                ? "dev.layr.fshost.fs not listed (\(mods.count) modules installed)"
                : mine.map { "\($0.bundleIdentifier) enabled=\($0.isEnabled)" }.joined(separator: "\n")
        } catch {
            status = "FSClient error: \(error.localizedDescription)"
        }
    }
}
