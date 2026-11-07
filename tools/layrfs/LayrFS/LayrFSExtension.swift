import ExtensionFoundation
import Foundation
import FSKit

@main
struct LayrFSExtension: UnaryFileSystemExtension {
    // One instance for the life of the extension process (the Xcode template builds a new one per access).
    let fileSystem = LayrFileSystem()
}
