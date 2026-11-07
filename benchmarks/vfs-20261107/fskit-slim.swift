import ExtensionFoundation
import FSKit.FSClient
import FSKit.FSContainer
import FSKit.FSContext
import FSKit.FSEntityIdentifier
import FSKit.FSFileName
import FSKit.FSFileSystem
import FSKit.FSFileSystemBase
import FSKit.FSFreeSpace
import FSKit.FSItem
import FSKit.FSKitDefines
import FSKit.FSKitError
import FSKit.FSKitFunctions
import FSKit.FSKitTypes
import FSKit.FSModuleIdentity
import FSKit.FSMutableFileDataBuffer
import FSKit.FSResource
import FSKit.FSTask
import FSKit.FSTaskOptions
import FSKit.FSUnaryFileSystem
import FSKit.FSVolume
import FSKit.FSVolumeDataCacheHandler
import FSKit.FSVolumeExtent
import FSKit.FSVolumeHandlerResult
import Foundation
//! Project version number for FSKit.
public var FSKitVersionNumber: Double
//! Project version string for FSKit.
public let FSKitVersionString: _
@available(macOS 15.4, *)
open class FSClient : NSObject {
    open class var shared: FSClient { get }
    @available(macOS 15.4, *)
    open func fetchInstalledExtensions(completionHandler: @escaping @Sendable ([FSModuleIdentity]?, (any Error)?) -> Void)
    @available(macOS 15.4, *)
    open var installedExtensions: [FSModuleIdentity] { get async throws }
    @available(macOS 27.0, *)
    open func mountSingleVolume(resource: FSResource, bundleID: String, options: [String], completionHandler: @escaping @Sendable (URL?, (any Error)?) -> Void)
    @available(macOS 27.0, *)
    open func mountSingleVolume(resource: FSResource, bundleID: String, options: [String]) async throws -> URL
    @available(macOS 27.0, *)
    open func openFileSystemExtensionsSettings() -> Bool
}
@available(macOS 15.4, *)
public enum FSContainerState : Int, @unchecked Sendable {
    case notReady = 0
    case blocked = 1
    case ready = 2
    case active = 3
}
open class FSContainerStatus : NSObject, NSCopying {
    open var state: FSContainerState { get }
    open var status: (any Error)? { get }
    open class var active: FSContainerStatus { get }
    open class func active(status errorStatus: any Error) -> Self
    open class func blocked(status errorStatus: any Error) -> Self
    open class func notReady(status errorStatus: any Error) -> Self
    open class var ready: FSContainerStatus { get }
    open class func ready(status errorStatus: any Error) -> Self
}
@available(macOS 15.4, *)
open class FSContainerIdentifier : FSEntityIdentifier {
    @NSCopying open var volumeIdentifier: FSVolume.Identifier { get }
}
@available(macOS 27.0, *)
open class FSContext : NSObject {
    open var realUserID: Int { get }
    open var effectiveUserID: Int { get }
    open var realGroupID: Int { get }
    open var effectiveGroupID: Int { get }
}
@available(macOS 15.4, *)
open class FSEntityIdentifier : NSObject, NSCopying, NSSecureCoding {
    public init()
    public init(uuid: UUID)
    public init(uuid: UUID, qualifier: UInt64)
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public init(uuid: UUID, data qualifierData: Data)
    @available(macOS 27.0, *)
    public init?(uuid: UUID, qualifierData: Data)
    open var uuid: UUID
    open var qualifier: Data?
}
@available(macOS 15.4, *)
extension FSEntityIdentifier : Identifiable, @unchecked Sendable {
    @available(macOS 15.4, *)
    public var id: String { get }
    @available(macOS 15.4, *)
    public typealias ID = String
}
@available(macOS 15.4, *)
open class FSFileName : NSObject, NSSecureCoding, NSCopying {
    @available(macOS 15.4, *)
    open var data: Data { get }
    @available(macOS 15.4, *)
    open var string: String? { get }
    @available(macOS 15.4, *)
    open var debugDescription: String { get }
    @available(macOS 15.4, *)
    public convenience init(data name: Data)
    @available(macOS 15.4, *)
    public convenience init(string name: String)
}
@available(macOS 15.4, *)
extension FSFileName {
    @available(macOS 15.4, *)
    public convenience init(cString name: UnsafeBufferPointer<CChar>)
    @available(macOS 15.4, *)
    public convenience init(bytes name: UnsafeBufferPointer<CChar>)
}
@available(macOS 15.4, *)
public protocol FSFileSystemBase : NSObjectProtocol {
    @available(macOS 15.4, *)
    @NSCopying var containerStatus: FSContainerStatus { get set }
    @available(macOS 15.4, *)
    func wipe(_ resource: FSBlockDeviceResource, completionHandler completion: @escaping @Sendable ((any Error)?) -> Void)
    @available(macOS 15.4, *)
    func wipe(_ resource: FSBlockDeviceResource) async throws
}
@available(macOS 27.0, *)
open class FSFreeSpace : NSObject {
    public init()
    open func populate(bytes freeSpaceBytes: UInt64)
    open class var noUpdate: FSFreeSpace { get }
}
@available(macOS 15.4, *)
open class FSItem : NSObject {
    @available(macOS 27.0, *)
    open func tryReclaim(_ reclaimBlock: @escaping () -> Void) -> Bool
}
extension FSItem {
    @available(macOS 15.4, *)
    open class Attributes : NSObject, NSSecureCoding {
        open func invalidateAllProperties()
        open var uid: UInt32
        open var gid: UInt32
        open var mode: UInt32
        open var type: FSItem.ItemType
        open var linkCount: UInt32
        open var flags: UInt32
        open var size: UInt64
        open var allocSize: UInt64
        open var fileID: FSItem.Identifier
        open var parentID: FSItem.Identifier
        open var supportsLimitedXAttrs: Bool
        open var inhibitKernelOffloadedIO: Bool
        open var modifyTime: timespec
        open var addedTime: timespec
        open var changeTime: timespec
        open var accessTime: timespec
        open var birthTime: timespec
        open var backupTime: timespec
        open func isValid(_ attribute: FSItem.Attribute) -> Bool
    }
    @available(macOS 15.4, *)
    open class SetAttributesRequest : FSItem.Attributes {
        open var consumedAttributes: FSItem.Attribute
        open func wasAttributeConsumed(_ attribute: FSItem.Attribute) -> Bool
    }
    @available(macOS 15.4, *)
    open class GetAttributesRequest : NSObject, NSSecureCoding {
        open var wantedAttributes: FSItem.Attribute
        open func isAttributeWanted(_ attribute: FSItem.Attribute) -> Bool
    }
    @available(macOS 15.4, *)
    public struct Attribute : OptionSet, @unchecked Sendable {
        public init(rawValue: Int)
        public static var type: FSItem.Attribute { get }
        public static var mode: FSItem.Attribute { get }
        public static var linkCount: FSItem.Attribute { get }
        public static var uid: FSItem.Attribute { get }
        public static var gid: FSItem.Attribute { get }
        public static var flags: FSItem.Attribute { get }
        public static var size: FSItem.Attribute { get }
        public static var allocSize: FSItem.Attribute { get }
        public static var fileID: FSItem.Attribute { get }
        public static var parentID: FSItem.Attribute { get }
        public static var accessTime: FSItem.Attribute { get }
        public static var modifyTime: FSItem.Attribute { get }
        public static var changeTime: FSItem.Attribute { get }
        public static var birthTime: FSItem.Attribute { get }
        public static var backupTime: FSItem.Attribute { get }
        public static var addedTime: FSItem.Attribute { get }
        public static var supportsLimitedXAttrs: FSItem.Attribute { get }
        public static var inhibitKernelOffloadedIO: FSItem.Attribute { get }
    }
    @available(macOS 15.4, *)
    public enum ItemType : Int, @unchecked Sendable {
        case unknown = 0
        case file = 1
        case directory = 2
        case symlink = 3
        case fifo = 4
        case charDevice = 5
        case blockDevice = 6
        case socket = 7
    }
    @available(macOS 15.4, *)
    public enum Identifier : UInt64, @unchecked Sendable {
        case invalid = 0
        case parentOfRoot = 1
        case rootDirectory = 2
    }
}
@available(macOS 15.4, *)
public let FSKitErrorDomain: String
public struct FSError : CustomNSError, Hashable, Error {
    public init(_nsError: NSError)
    public static var errorDomain: String { get }
    public enum Code : Int, @unchecked Sendable, Equatable {
        public typealias _ErrorType = FSError
        case moduleLoadFailed = 4500
        case resourceUnrecognized = 4501
        case resourceDamaged = 4502
        case resourceUnusable = 4503
        case statusOperationInProgress = 4504
        case statusOperationPaused = 4505
        case invalidDirectoryCookie = 4506
    }
    public static var moduleLoadFailed: FSError.Code { get }
    public static var resourceUnrecognized: FSError.Code { get }
    public static var resourceDamaged: FSError.Code { get }
    public static var resourceUnusable: FSError.Code { get }
    public static var statusOperationInProgress: FSError.Code { get }
    public static var statusOperationPaused: FSError.Code { get }
    public static var invalidDirectoryCookie: FSError.Code { get }
}
@available(macOS 27.0, *)
public struct FSDataCacheError : CustomNSError, Hashable, Error {
    public init(_nsError: NSError)
    public static var errorDomain: String { get }
    @available(macOS 27.0, *)
    public enum Code : Int, @unchecked Sendable, Equatable {
        public typealias _ErrorType = FSDataCacheError
        case errorInvalidCacheModeCoherency = 4510
        case errorInvalidCacheTransition = 4511
        case errorCacheFlushFailed = 4512
        case errorCacheInvalidationFailed = 4513
        case errorCacheOperationConflict = 4514
    }
    public static var errorInvalidCacheModeCoherency: FSDataCacheError.Code { get }
    public static var errorInvalidCacheTransition: FSDataCacheError.Code { get }
    public static var errorCacheFlushFailed: FSDataCacheError.Code { get }
    public static var errorCacheInvalidationFailed: FSDataCacheError.Code { get }
    public static var errorCacheOperationConflict: FSDataCacheError.Code { get }
}
@available(macOS 15.4, *)
public func fs_errorForPOSIXError(_: Int32) -> any Error
@available(macOS 15.4, *)
public func fs_errorForMachError(_ errorCode: Int32) -> any Error
@available(macOS 15.4, *)
public func fs_errorForCocoaError(_ errorCode: Int32) -> any Error
@available(macOS 15.4, *)
open class FSModuleIdentity : NSObject {
    @available(macOS 15.4, *)
    open var bundleIdentifier: String { get }
    @available(macOS 15.4, *)
    open var url: URL { get }
    @available(macOS 15.4, *)
    open var isEnabled: Bool { get }
}
@available(macOS 15.4, *)
extension FSModuleIdentity : Identifiable {
    @available(macOS 15.4, *)
    public var id: String { get }
    @available(macOS 15.4, *)
    public typealias ID = String
}
@available(macOS 15.4, *)
open class FSMutableFileDataBuffer : NSObject {
    @available(macOS 15.4, *)
    open var length: Int { get }
}
@available(macOS 15.4, *)
extension FSMutableFileDataBuffer {
    @available(macOS 15.4, *)
    public func withUnsafeMutableBytes<R, E>(_ body: (UnsafeMutableRawBufferPointer) throws(E) -> R) throws(E) -> R where E : Error
    @available(macOS 27.0, *)
    public func createMutableRawSpan() throws -> MutableRawSpan
}
@available(macOS 15.4, *)
public enum FSMatchResult : Int, @unchecked Sendable {
    case notRecognized = 0
    case recognized = 1
    case usableButLimited = 2
    case usable = 3
}
@available(macOS 15.4, *)
open class FSResource : NSObject, NSSecureCoding {
    open var isRevoked: Bool { get }
    open func makeProxy() -> Self
    open func revoke()
}
open class FSMetadataRange : NSObject {
    open var startOffset: off_t { get }
    open var segmentLength: UInt64 { get }
    open var segmentCount: UInt64 { get }
    public init(offset startOffset: off_t, segmentLength: UInt64, segmentCount: UInt64)
}
@available(macOS 15.4, *)
open class FSBlockDeviceResource : FSResource {
    open var bsdName: String { get }
    open var isWritable: Bool { get }
    open var blockSize: UInt64 { get }
    open var blockCount: UInt64 { get }
    open var physicalBlockSize: UInt64 { get }
    @available(macOS 15.4, *)
    open func metadataFlush() throws
    @available(macOS 15.4, *)
    open func asynchronousMetadataFlush() throws
    @available(macOS 15.4, *)
    open func metadataClear(_ rangesToClear: [FSMetadataRange], withDelayedWrites: Bool) throws
    @available(macOS 15.4, *)
    open func metadataPurge(_ rangesToPurge: [FSMetadataRange]) throws
}
@available(macOS 15.4, *)
extension FSBlockDeviceResource {
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) async throws -> Int
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int, completionHandler: @escaping (Int, (any Error)?) -> Void)
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) throws -> Int
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) async throws -> Int
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int, completionHandler: @escaping (Int, (any Error)?) -> Void)
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws -> Int
    @available(macOS 15.4, *)
    public func metadataRead(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) throws
    @available(macOS 15.4, *)
    public func metadataWrite(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws
    @available(macOS 15.4, *)
    public func delayedMetadataWrite(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws
}
@available(macOS 26.0, *)
open class FSGenericURLResource : FSResource {
    open var url: URL { get }
    public init(url: URL)
}
@available(macOS 26.0, *)
open class FSPathURLResource : FSResource {
    open var url: URL { get }
    public init(url URL: URL, writable: Bool)
    open var isWritable: Bool { get }
}
@available(macOS 15.4, *)
public protocol FSManageableResourceMaintenanceOperations : NSObjectProtocol {
    @available(macOS 15.4, *)
    func startCheck(task: FSTask, options: FSTaskOptions) throws -> Progress
    @available(macOS 15.4, *)
    func startFormat(task: FSTask, options: FSTaskOptions) throws -> Progress
}
@available(macOS 15.4, *)
open class FSProbeResult : NSObject, NSSecureCoding {
    open var result: FSMatchResult { get }
    open var name: String? { get }
    open var containerID: FSContainerIdentifier? { get }
    open class var notRecognized: FSProbeResult { get }
    open class func recognized(name: String, containerID: FSContainerIdentifier) -> Self
    open class var usableButLimited: FSProbeResult { get }
    open class func usableButLimited(name: String, containerID: FSContainerIdentifier) -> Self
    open class func usable(name: String, containerID: FSContainerIdentifier) -> Self
}
@available(macOS 15.4, *)
open class FSTask : NSObject, @unchecked Sendable {
    @available(macOS 15.4, *)
    open func logMessage(_ str: String)
    @available(macOS 15.4, *)
    open func didComplete(error: (any Error)?)
    @available(macOS 26.0, *)
    open var cancellationHandler: (@Sendable () -> (any Error)?)?
}
@available(macOS 15.4, *)
open class FSTaskOptions : NSObject {
    open var taskOptions: [String] { get }
    open func url(forOption option: String) -> URL?
}
@available(macOS 15.4, *)
open class FSUnaryFileSystem : NSObject, FSFileSystemBase {
}
@available(macOS 15.4, *)
public protocol FSUnaryFileSystemOperations : NSObjectProtocol {
    @available(macOS 15.4, *)
    func probeResource(resource: FSResource, replyHandler reply: @escaping @Sendable (FSProbeResult?, (any Error)?) -> Void)
    @available(macOS 15.4, *)
    func probeResource(resource: FSResource) async throws -> FSProbeResult
    @available(macOS 15.4, *)
    func loadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void)
    @available(macOS 15.4, *)
    func loadResource(resource: FSResource, options: FSTaskOptions) async throws -> FSVolume
    @available(macOS 15.4, *)
    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    @available(macOS 15.4, *)
    func unloadResource(resource: FSResource, options: FSTaskOptions) async throws
    @available(macOS 15.4, *)
    optional func didFinishLoading()
}
@available(macOS 15.4, *)
public struct FSDirectoryCookie : Hashable, Equatable, RawRepresentable, @unchecked Sendable {
    public init(_ rawValue: UInt64)
    public init(rawValue: UInt64)
}
extension FSDirectoryCookie {
    @available(macOS 15.4, *)
    public static let initial: FSDirectoryCookie
}
@available(macOS 15.4, *)
public struct FSDirectoryVerifier : Hashable, Equatable, RawRepresentable, @unchecked Sendable {
    public init(_ rawValue: UInt64)
    public init(rawValue: UInt64)
}
extension FSDirectoryVerifier {
    @available(macOS 15.4, *)
    public static let initial: FSDirectoryVerifier
}
@available(macOS 15.4, *)
public struct FSDeactivateOptions : OptionSet, @unchecked Sendable {
    public init(rawValue: Int)
    public static var force: FSDeactivateOptions { get }
}
@available(macOS 15.4, *)
public enum FSSyncFlags : Int, @unchecked Sendable {
    case wait = 1
    case noWait = 2
    case dWait = 4
}
@available(macOS 15.4, *)
open class FSDirectoryEntryPacker : NSObject {
    @available(macOS 15.4, *)
    open func packEntry(name: FSFileName, itemType: FSItem.ItemType, itemID: FSItem.Identifier, nextCookie: FSDirectoryCookie, attributes: FSItem.Attributes?) -> Bool
}
@available(macOS 15.4, *)
open class FSVolume : NSObject {
    open var volumeID: FSVolume.Identifier { get }
    @NSCopying open var name: FSFileName
    public init(volumeID: FSVolume.Identifier, volumeName: FSFileName)
}
@available(macOS 15.4, *)
extension FSVolume : Identifiable {
    @available(macOS 15.4, *)
    public var id: String { get }
    @available(macOS 15.4, *)
    public typealias ID = String
}
extension FSVolume {
    @available(macOS 15.4, *)
    public protocol PathConfOperations : NSObjectProtocol {
        var maximumLinkCount: Int { get }
        var maximumNameLength: Int { get }
        var restrictsOwnershipChanges: Bool { get }
        var truncatesLongNames: Bool { get }
        optional var maximumXattrSize: Int { get }
        optional var maximumXattrSizeInBits: Int { get }
        optional var maximumFileSize: UInt64 { get }
        optional var maximumFileSizeInBits: Int { get }
    }
    @available(macOS 15.4, *)
    public protocol CommonOperations {
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }
        var volumeStatistics: FSStatFSResult { get }
        @available(macOS 26.0, *)
        optional var enableOpenUnlinkEmulation: Bool { get }
        @available(macOS 26.4, *)
        optional var requestedMountOptions: FSVolume.MountOptions { get }
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func mount(options: FSTaskOptions) async throws
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)
        func unmount() async
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func synchronize(flags: FSSyncFlags) async throws
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func reclaimItem(_ item: FSItem) async throws
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol Operations : FSVolume.CommonOperations, FSVolume.PathConfOperations {
        func activate(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSItem?, (any Error)?) -> Void)
        func activate(options: FSTaskOptions) async throws -> FSItem
        func deactivate(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivate(options: FSDeactivateOptions = []) async throws
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem) async throws -> (FSItem, FSFileName)
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest) async throws -> (FSItem, FSFileName)
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName) async throws -> (FSItem, FSFileName)
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem) async throws -> FSFileName
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?) async throws -> FSFileName
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem) async throws
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem) async throws -> FSItem.Attributes
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem) async throws -> FSItem.Attributes
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, replyHandler reply: @escaping @Sendable (FSDirectoryVerifier, (any Error)?) -> Void)
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker) async throws -> FSDirectoryVerifier
        func readSymbolicLink(_ item: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func readSymbolicLink(_ item: FSItem) async throws -> FSFileName
    }
    @available(macOS 27.0, *)
    public protocol Handler : FSVolume.CommonOperations, FSVolume.PathConfOperations {
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }
        var volumeStatistics: FSStatFSResult { get }
        func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void)
        func activateVolume(options: FSTaskOptions) async throws -> FSActivateResult
        func deactivateVolume(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivateVolume(options: FSDeactivateOptions = []) async throws
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func mount(options: FSTaskOptions) async throws
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)
        func unmount() async
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func synchronize(flags: FSSyncFlags) async throws
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSLookupItemResult
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func reclaimItem(_ item: FSItem) async throws
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext) async throws -> FSCreateItemResult
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext) async throws -> FSCreateSymlinkResult
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSCreateLinkResult
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext) async throws -> FSRenameItemResult
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext) async throws -> FSRemoveItemResult
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext) async throws -> FSGetAttributesResult
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext) async throws -> FSSetAttributesResult
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext) async throws -> FSEnumerateDirectoryResult
        func readSymbolicLink(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)
        func readSymbolicLink(_ item: FSItem, context: FSContext) async throws -> FSReadSymlinkResult
    }
    @available(macOS 15.4, *)
    public enum SetXattrPolicy : UInt, @unchecked Sendable {
        case alwaysSet = 0
        case mustCreate = 1
        case mustReplace = 2
        case delete = 3
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol XattrOperations : NSObjectProtocol {
        optional var xattrOperationsInhibited: Bool { get set }
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]
        func getXattr(named name: FSFileName, of item: FSItem, replyHandler reply: @escaping @Sendable (Data?, (any Error)?) -> Void)
        func xattr(named name: FSFileName, of item: FSItem) async throws -> Data
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy) async throws
        func listXattrs(of item: FSItem, replyHandler reply: @escaping @Sendable ([FSFileName]?, (any Error)?) -> Void)
        func xattrs(of item: FSItem) async throws -> [FSFileName]
    }
    @available(macOS 27.0, *)
    public protocol XattrHandler : NSObjectProtocol {
        optional var xattrOperationsInhibited: Bool { get }
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]
        func getXattr(named name: FSFileName, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)
        func xattr(named name: FSFileName, of item: FSItem, context: FSContext) async throws -> FSGetXattrResult
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext) async throws -> FSSetXattrResult
        func listXattrs(of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)
        func xattrs(of item: FSItem, context: FSContext) async throws -> FSListXattrsResult
    }
    @available(macOS 15.4, *)
    public struct OpenModes : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var read: FSVolume.OpenModes { get }
        public static var write: FSVolume.OpenModes { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol OpenCloseOperations : NSObjectProtocol {
        optional var isOpenCloseInhibited: Bool { get set }
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
    }
    @available(macOS 27.0, *)
    public protocol OpenCloseHandler : NSObjectProtocol {
        optional var isOpenCloseInhibited: Bool { get }
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ReadWriteOperations : NSObjectProtocol {
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> Int
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int
    }
    @available(macOS 27.0, *)
    public protocol ReadWriteHandler : NSObjectProtocol {
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> FSReadFileResult
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> FSWriteFileResult
    }
    @available(macOS 15.4, *)
    public struct AccessMask : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var readData: FSVolume.AccessMask { get }
        public static var listDirectory: FSVolume.AccessMask { get }
        public static var writeData: FSVolume.AccessMask { get }
        public static var addFile: FSVolume.AccessMask { get }
        public static var execute: FSVolume.AccessMask { get }
        public static var search: FSVolume.AccessMask { get }
        public static var delete: FSVolume.AccessMask { get }
        public static var appendData: FSVolume.AccessMask { get }
        public static var addSubdirectory: FSVolume.AccessMask { get }
        public static var deleteChild: FSVolume.AccessMask { get }
        public static var readAttributes: FSVolume.AccessMask { get }
        public static var writeAttributes: FSVolume.AccessMask { get }
        public static var readXattr: FSVolume.AccessMask { get }
        public static var writeXattr: FSVolume.AccessMask { get }
        public static var readSecurity: FSVolume.AccessMask { get }
        public static var writeSecurity: FSVolume.AccessMask { get }
        public static var takeOwnership: FSVolume.AccessMask { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol AccessCheckOperations : NSObjectProtocol {
        optional var isAccessCheckInhibited: Bool { get set }
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, replyHandler reply: @escaping @Sendable (Bool, (any Error)?) -> Void)
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask) async throws -> Bool
    }
    @available(macOS 27.0, *)
    public protocol AccessCheckHandler : NSObjectProtocol {
        optional var isAccessCheckInhibited: Bool { get }
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext, replyHandler reply: @escaping @Sendable (FSCheckAccessResult?, (any Error)?) -> Void)
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext) async throws -> FSCheckAccessResult
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol RenameOperations : NSObjectProtocol {
        optional var isVolumeRenameInhibited: Bool { get set }
        func setVolumeName(_ name: FSFileName, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func setVolumeName(_ name: FSFileName) async throws -> FSFileName
    }
    @available(macOS 27.0, *)
    public protocol RenameHandler : NSObjectProtocol {
        optional var isVolumeRenameInhibited: Bool { get }
        func setVolumeName(_ name: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSVolumeRenameResult?, (any Error)?) -> Void)
        func setVolumeName(_ name: FSFileName, context: FSContext) async throws -> FSVolumeRenameResult
    }
    @available(macOS 15.4, *)
    public struct PreallocateFlags : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var contiguous: FSVolume.PreallocateFlags { get }
        public static var all: FSVolume.PreallocateFlags { get }
        public static var persist: FSVolume.PreallocateFlags { get }
        public static var fromEOF: FSVolume.PreallocateFlags { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol PreallocateOperations : NSObjectProtocol {
        optional var isPreallocateInhibited: Bool { get set }
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags) async throws -> Int
    }
    @available(macOS 27.0, *)
    public protocol PreallocateHandler : NSObjectProtocol {
        optional var isPreallocateInhibited: Bool { get }
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void)
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext) async throws -> FSPreallocateResult
    }
    @available(macOS 15.4, *)
    public struct ItemDeactivationOptions : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var always: FSVolume.ItemDeactivationOptions { get }
        public static var forRemovedItems: FSVolume.ItemDeactivationOptions { get }
        public static var forPreallocatedItems: FSVolume.ItemDeactivationOptions { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ItemDeactivation : NSObjectProtocol {
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }
        func deactivateItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivateItem(_ item: FSItem) async throws
    }
    @available(macOS 27.0, *)
    public protocol ItemDeactivationHandler : NSObjectProtocol {
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }
        func deactivateItem(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSDeactivateItemResult?, (any Error)?) -> Void)
        func deactivateItem(_ item: FSItem, context: FSContext) async throws -> FSDeactivateItemResult
    }
    @available(macOS 27.0, *)
    public enum SeekRegion : UInt, @unchecked Sendable {
        case hole = 1
        case data = 2
    }
    @available(macOS 27.0, *)
    public protocol SeekRegionHandler : NSObjectProtocol {
        optional var isSeekRegionInhibited: Bool { get }
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext, replyHandler reply: @escaping @Sendable (FSSeekRegionResult?, (any Error)?) -> Void)
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext) async throws -> FSSeekRegionResult
    }
    @available(macOS 15.4, *)
    open class Identifier : FSEntityIdentifier {
    }
    @available(macOS 26.4, *)
    public struct MountOptions : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var readOnly: FSVolume.MountOptions { get }
    }
    @available(macOS 15.4, *)
    public enum CaseFormat : Int, @unchecked Sendable {
        case sensitive = 0
        case insensitive = 1
        case insensitiveCasePreserving = 2
    }
    @available(macOS 15.4, *)
    open class SupportedCapabilities : NSObject, NSSecureCoding {
        open var supportsPersistentObjectIDs: Bool
        open var supportsSymbolicLinks: Bool
        open var supportsHardLinks: Bool
        open var supportsJournal: Bool
        open var supportsActiveJournal: Bool
        open var doesNotSupportRootTimes: Bool
        open var supportsSparseFiles: Bool
        open var supportsZeroRuns: Bool
        open var supportsFastStatFS: Bool
        open var supports2TBFiles: Bool
        open var supportsOpenDenyModes: Bool
        open var supportsHiddenFiles: Bool
        open var doesNotSupportVolumeSizes: Bool
        open var supports64BitObjectIDs: Bool
        open var supportsDocumentID: Bool
        open var doesNotSupportImmutableFiles: Bool
        open var doesNotSupportSettingFilePermissions: Bool
        open var supportsSharedSpace: Bool
        open var supportsVolumeGroups: Bool
        open var caseFormat: FSVolume.CaseFormat
    }
}
extension FSVolume {
    @available(macOS 15.4, *)
    public protocol PathConfOperations : NSObjectProtocol {
        var maximumLinkCount: Int { get }
        var maximumNameLength: Int { get }
        var restrictsOwnershipChanges: Bool { get }
        var truncatesLongNames: Bool { get }
        optional var maximumXattrSize: Int { get }
        optional var maximumXattrSizeInBits: Int { get }
        optional var maximumFileSize: UInt64 { get }
        optional var maximumFileSizeInBits: Int { get }
    }
    @available(macOS 15.4, *)
    public protocol CommonOperations {
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }
        var volumeStatistics: FSStatFSResult { get }
        @available(macOS 26.0, *)
        optional var enableOpenUnlinkEmulation: Bool { get }
        @available(macOS 26.4, *)
        optional var requestedMountOptions: FSVolume.MountOptions { get }
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func mount(options: FSTaskOptions) async throws
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)
        func unmount() async
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func synchronize(flags: FSSyncFlags) async throws
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func reclaimItem(_ item: FSItem) async throws
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol Operations : FSVolume.CommonOperations, FSVolume.PathConfOperations {
        func activate(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSItem?, (any Error)?) -> Void)
        func activate(options: FSTaskOptions) async throws -> FSItem
        func deactivate(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivate(options: FSDeactivateOptions = []) async throws
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem) async throws -> (FSItem, FSFileName)
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest) async throws -> (FSItem, FSFileName)
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName) async throws -> (FSItem, FSFileName)
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem) async throws -> FSFileName
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?) async throws -> FSFileName
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem) async throws
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem) async throws -> FSItem.Attributes
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem) async throws -> FSItem.Attributes
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, replyHandler reply: @escaping @Sendable (FSDirectoryVerifier, (any Error)?) -> Void)
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker) async throws -> FSDirectoryVerifier
        func readSymbolicLink(_ item: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func readSymbolicLink(_ item: FSItem) async throws -> FSFileName
    }
    @available(macOS 27.0, *)
    public protocol Handler : FSVolume.CommonOperations, FSVolume.PathConfOperations {
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }
        var volumeStatistics: FSStatFSResult { get }
        func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void)
        func activateVolume(options: FSTaskOptions) async throws -> FSActivateResult
        func deactivateVolume(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivateVolume(options: FSDeactivateOptions = []) async throws
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func mount(options: FSTaskOptions) async throws
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)
        func unmount() async
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func synchronize(flags: FSSyncFlags) async throws
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSLookupItemResult
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func reclaimItem(_ item: FSItem) async throws
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext) async throws -> FSCreateItemResult
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext) async throws -> FSCreateSymlinkResult
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSCreateLinkResult
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext) async throws -> FSRenameItemResult
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext) async throws -> FSRemoveItemResult
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext) async throws -> FSGetAttributesResult
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext) async throws -> FSSetAttributesResult
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext) async throws -> FSEnumerateDirectoryResult
        func readSymbolicLink(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)
        func readSymbolicLink(_ item: FSItem, context: FSContext) async throws -> FSReadSymlinkResult
    }
    @available(macOS 15.4, *)
    public enum SetXattrPolicy : UInt, @unchecked Sendable {
        case alwaysSet = 0
        case mustCreate = 1
        case mustReplace = 2
        case delete = 3
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol XattrOperations : NSObjectProtocol {
        optional var xattrOperationsInhibited: Bool { get set }
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]
        func getXattr(named name: FSFileName, of item: FSItem, replyHandler reply: @escaping @Sendable (Data?, (any Error)?) -> Void)
        func xattr(named name: FSFileName, of item: FSItem) async throws -> Data
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy) async throws
        func listXattrs(of item: FSItem, replyHandler reply: @escaping @Sendable ([FSFileName]?, (any Error)?) -> Void)
        func xattrs(of item: FSItem) async throws -> [FSFileName]
    }
    @available(macOS 27.0, *)
    public protocol XattrHandler : NSObjectProtocol {
        optional var xattrOperationsInhibited: Bool { get }
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]
        func getXattr(named name: FSFileName, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)
        func xattr(named name: FSFileName, of item: FSItem, context: FSContext) async throws -> FSGetXattrResult
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext) async throws -> FSSetXattrResult
        func listXattrs(of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)
        func xattrs(of item: FSItem, context: FSContext) async throws -> FSListXattrsResult
    }
    @available(macOS 15.4, *)
    public struct OpenModes : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var read: FSVolume.OpenModes { get }
        public static var write: FSVolume.OpenModes { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol OpenCloseOperations : NSObjectProtocol {
        optional var isOpenCloseInhibited: Bool { get set }
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
    }
    @available(macOS 27.0, *)
    public protocol OpenCloseHandler : NSObjectProtocol {
        optional var isOpenCloseInhibited: Bool { get }
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ReadWriteOperations : NSObjectProtocol {
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> Int
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int
    }
    @available(macOS 27.0, *)
    public protocol ReadWriteHandler : NSObjectProtocol {
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> FSReadFileResult
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> FSWriteFileResult
    }
    @available(macOS 15.4, *)
    public struct AccessMask : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var readData: FSVolume.AccessMask { get }
        public static var listDirectory: FSVolume.AccessMask { get }
        public static var writeData: FSVolume.AccessMask { get }
        public static var addFile: FSVolume.AccessMask { get }
        public static var execute: FSVolume.AccessMask { get }
        public static var search: FSVolume.AccessMask { get }
        public static var delete: FSVolume.AccessMask { get }
        public static var appendData: FSVolume.AccessMask { get }
        public static var addSubdirectory: FSVolume.AccessMask { get }
        public static var deleteChild: FSVolume.AccessMask { get }
        public static var readAttributes: FSVolume.AccessMask { get }
        public static var writeAttributes: FSVolume.AccessMask { get }
        public static var readXattr: FSVolume.AccessMask { get }
        public static var writeXattr: FSVolume.AccessMask { get }
        public static var readSecurity: FSVolume.AccessMask { get }
        public static var writeSecurity: FSVolume.AccessMask { get }
        public static var takeOwnership: FSVolume.AccessMask { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol AccessCheckOperations : NSObjectProtocol {
        optional var isAccessCheckInhibited: Bool { get set }
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, replyHandler reply: @escaping @Sendable (Bool, (any Error)?) -> Void)
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask) async throws -> Bool
    }
    @available(macOS 27.0, *)
    public protocol AccessCheckHandler : NSObjectProtocol {
        optional var isAccessCheckInhibited: Bool { get }
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext, replyHandler reply: @escaping @Sendable (FSCheckAccessResult?, (any Error)?) -> Void)
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext) async throws -> FSCheckAccessResult
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol RenameOperations : NSObjectProtocol {
        optional var isVolumeRenameInhibited: Bool { get set }
        func setVolumeName(_ name: FSFileName, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)
        func setVolumeName(_ name: FSFileName) async throws -> FSFileName
    }
    @available(macOS 27.0, *)
    public protocol RenameHandler : NSObjectProtocol {
        optional var isVolumeRenameInhibited: Bool { get }
        func setVolumeName(_ name: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSVolumeRenameResult?, (any Error)?) -> Void)
        func setVolumeName(_ name: FSFileName, context: FSContext) async throws -> FSVolumeRenameResult
    }
    @available(macOS 15.4, *)
    public struct PreallocateFlags : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var contiguous: FSVolume.PreallocateFlags { get }
        public static var all: FSVolume.PreallocateFlags { get }
        public static var persist: FSVolume.PreallocateFlags { get }
        public static var fromEOF: FSVolume.PreallocateFlags { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol PreallocateOperations : NSObjectProtocol {
        optional var isPreallocateInhibited: Bool { get set }
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags) async throws -> Int
    }
    @available(macOS 27.0, *)
    public protocol PreallocateHandler : NSObjectProtocol {
        optional var isPreallocateInhibited: Bool { get }
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void)
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext) async throws -> FSPreallocateResult
    }
    @available(macOS 15.4, *)
    public struct ItemDeactivationOptions : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var always: FSVolume.ItemDeactivationOptions { get }
        public static var forRemovedItems: FSVolume.ItemDeactivationOptions { get }
        public static var forPreallocatedItems: FSVolume.ItemDeactivationOptions { get }
    }
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ItemDeactivation : NSObjectProtocol {
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }
        func deactivateItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
        func deactivateItem(_ item: FSItem) async throws
    }
    @available(macOS 27.0, *)
    public protocol ItemDeactivationHandler : NSObjectProtocol {
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }
        func deactivateItem(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSDeactivateItemResult?, (any Error)?) -> Void)
        func deactivateItem(_ item: FSItem, context: FSContext) async throws -> FSDeactivateItemResult
    }
    @available(macOS 27.0, *)
    public enum SeekRegion : UInt, @unchecked Sendable {
        case hole = 1
        case data = 2
    }
    @available(macOS 27.0, *)
    public protocol SeekRegionHandler : NSObjectProtocol {
        optional var isSeekRegionInhibited: Bool { get }
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext, replyHandler reply: @escaping @Sendable (FSSeekRegionResult?, (any Error)?) -> Void)
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext) async throws -> FSSeekRegionResult
    }
    @available(macOS 15.4, *)
    open class Identifier : FSEntityIdentifier {
    }
    @available(macOS 26.4, *)
    public struct MountOptions : OptionSet, @unchecked Sendable {
        public init(rawValue: UInt)
        public static var readOnly: FSVolume.MountOptions { get }
    }
    @available(macOS 15.4, *)
    public enum CaseFormat : Int, @unchecked Sendable {
        case sensitive = 0
        case insensitive = 1
        case insensitiveCasePreserving = 2
    }
    @available(macOS 15.4, *)
    open class SupportedCapabilities : NSObject, NSSecureCoding {
        open var supportsPersistentObjectIDs: Bool
        open var supportsSymbolicLinks: Bool
        open var supportsHardLinks: Bool
        open var supportsJournal: Bool
        open var supportsActiveJournal: Bool
        open var doesNotSupportRootTimes: Bool
        open var supportsSparseFiles: Bool
        open var supportsZeroRuns: Bool
        open var supportsFastStatFS: Bool
        open var supports2TBFiles: Bool
        open var supportsOpenDenyModes: Bool
        open var supportsHiddenFiles: Bool
        open var doesNotSupportVolumeSizes: Bool
        open var supports64BitObjectIDs: Bool
        open var supportsDocumentID: Bool
        open var doesNotSupportImmutableFiles: Bool
        open var doesNotSupportSettingFilePermissions: Bool
        open var supportsSharedSpace: Bool
        open var supportsVolumeGroups: Bool
        open var caseFormat: FSVolume.CaseFormat
    }
}
@available(macOS 15.4, *)
open class FSStatFSResult : NSObject, NSSecureCoding {
    open var blockSize: Int
    open var ioSize: Int
    open var totalBlocks: UInt64
    open var availableBlocks: UInt64
    open var freeBlocks: UInt64
    open var usedBlocks: UInt64
    open var totalBytes: UInt64
    open var availableBytes: UInt64
    open var freeBytes: UInt64
    open var usedBytes: UInt64
    open var totalFiles: UInt64
    open var freeFiles: UInt64
    open var fileSystemSubType: Int
    open var fileSystemTypeName: String { get }
    public init(fileSystemTypeName: String)
}
@available(macOS 27.0, *)
extension FSVolume {
    @available(macOS 27.0, *)
    open func setCacheState(for item: FSItem, cacheMode: FSVolume.DataCacheMode, coherencyType: FSVolume.KernelCacheCoherencyType, action: FSVolume.KernelCacheCoherencyAction) -> (any Error)?
}
extension FSVolume {
    @available(macOS 27.0, *)
    public enum DataCacheMode : Int, @unchecked Sendable {
        case none = 0
        case readWithCache = 1
        case readWriteWithCache = 2
    }
    @available(macOS 27.0, *)
    public enum KernelCacheCoherencyType : Int, @unchecked Sendable {
        case noCache = 0
        case readCache = 1
        case writeThrough = 2
        case writeBack = 3
    }
    @available(macOS 27.0, *)
    public enum KernelCacheCoherencyAction : Int, @unchecked Sendable {
        case push = 0
        case pushInvalidate = 1
        case invalidate = 2
        case update = 3
        case revoke = 4
    }
    @available(macOS 27.0, *)
    public protocol DataCacheHandler : NSObjectProtocol {
        @available(macOS 27.0, *)
        optional var isDataCacheInhibited: Bool { get }
        @available(macOS 27.0, *)
        func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSOpenItemResult?, (any Error)?) -> Void)
        @available(macOS 27.0, *)
        func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext) async throws -> FSOpenItemResult
        @available(macOS 27.0, *)
        func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void)
        @available(macOS 27.0, *)
        func close(_ item: FSItem, context: FSContext) async
        @available(macOS 27.0, *)
        func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSUpgradeItemResult?, (any Error)?) -> Void)
        @available(macOS 27.0, *)
        func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext) async throws -> FSUpgradeItemResult
    }
}
@available(macOS 15.4, *)
public struct FSOperationID : Hashable, Equatable, RawRepresentable, @unchecked Sendable {
    public init(_ rawValue: Int)
    public init(rawValue: Int)
}
extension FSOperationID {
    @available(macOS 15.4, *)
    public static let unspecified: FSOperationID
}
@available(macOS 15.4, *)
public struct FSBlockmapFlags : OptionSet, @unchecked Sendable {
    public init(rawValue: UInt)
    public static var read: FSBlockmapFlags { get }
    public static var write: FSBlockmapFlags { get }
}
@available(macOS 15.4, *)
public struct FSCompleteIOFlags : OptionSet, @unchecked Sendable {
    public init(rawValue: UInt)
    public static var read: FSCompleteIOFlags { get }
    public static var write: FSCompleteIOFlags { get }
    public static var async: FSCompleteIOFlags { get }
}
@available(macOS 15.4, *)
public enum FSExtentType : Int, @unchecked Sendable {
    case data = 0
    case zeroFill = 1
    @available(macOS 27.0, *)
    case readOnly = 2
}
@available(macOS 15.4, *)
open class FSExtentPacker : NSObject {
    open func packExtent(resource: FSBlockDeviceResource, type: FSExtentType, logicalOffset: off_t, physicalOffset: off_t, length: Int) -> Bool
}
@available(macOS, introduced: 15.4, deprecated: 27.0)
public protocol FSVolumeKernelOffloadedIOOperations : NSObjectProtocol {
    func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker) async throws
    func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)
    func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID) async throws
    func createFile(name: FSFileName, in directory: FSItem, attributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
    func createFile(name: FSFileName, in directory: FSItem, attributes: FSItem.SetAttributesRequest, packer: FSExtentPacker) async throws -> (FSItem, FSFileName)
    func lookupItem(name: FSFileName, in directory: FSItem, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)
    func lookupItem(name: FSFileName, in directory: FSItem, packer: FSExtentPacker) async throws -> (FSItem, FSFileName)
    optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)
    optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker) async throws -> Int
}
extension FSVolume {
    @available(macOS 27.0, *)
    public protocol KernelOffloadedIOHandler : NSObjectProtocol {
        func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSBlockmapResult?, (any Error)?) -> Void)
        func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker) async throws -> FSBlockmapResult
        func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID, replyHandler reply: @escaping @Sendable (FSCompleteIOResult?, (any Error)?) -> Void)
        func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID) async throws -> FSCompleteIOResult
        func createFile(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateFileKOIOResult?, (any Error)?) -> Void)
        func createFile(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, context: FSContext) async throws -> FSCreateFileKOIOResult
        func lookupItem(named name: FSFileName, in directory: FSItem, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemKOIOResult?, (any Error)?) -> Void)
        func lookupItem(named name: FSFileName, in directory: FSItem, packer: FSExtentPacker, context: FSContext) async throws -> FSLookupItemKOIOResult
        optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateKOIOResult?, (any Error)?) -> Void)
        optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, context: FSContext) async throws -> FSPreallocateKOIOResult
    }
}
@available(macOS 27.0, *)
open class FSVolumeHandlerResult : NSObject {
    open class var requestedAttributes: FSItem.GetAttributesRequest { get }
}
@available(macOS 27.0, *)
open class FSActivateResult : FSVolumeHandlerResult {
    public init?(rootItem: FSItem)
}
@available(macOS 27.0, *)
open class FSLookupItemResult : FSVolumeHandlerResult {
    public init?(foundItem: FSItem, itemName: FSFileName, itemAttributes: FSItem.Attributes)
}
@available(macOS 27.0, *)
open class FSCreateItemResult : FSVolumeHandlerResult {
    public init?(newItem: FSItem, newItemName: FSFileName, newItemAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSCreateSymlinkResult : FSCreateItemResult {
}
@available(macOS 27.0, *)
open class FSCreateLinkResult : FSVolumeHandlerResult {
    public init?(linkName: FSFileName, linkAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSRenameItemResult : FSVolumeHandlerResult {
    public init?(newName: FSFileName, renamedItemAttributes: FSItem.Attributes, sourceDirectoryAttributes: FSItem.Attributes, destinationDirectoryAttributes: FSItem.Attributes, overItemAttributes: FSItem.Attributes?, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSRemoveItemResult : FSVolumeHandlerResult {
    public init?(itemAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSGetAttributesResult : FSVolumeHandlerResult {
    public init?(attributes: FSItem.Attributes)
}
@available(macOS 27.0, *)
open class FSSetAttributesResult : FSVolumeHandlerResult {
    public init?(attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSEnumerateDirectoryResult : FSVolumeHandlerResult {
    public init?(verifier currentVerifier: UInt64)
}
@available(macOS 27.0, *)
open class FSReadSymlinkResult : FSVolumeHandlerResult {
    public init?(contents: FSFileName, symlinkAttributes attributes: FSItem.Attributes)
}
@available(macOS 27.0, *)
open class FSGetXattrResult : FSVolumeHandlerResult {
    public init?(xattrValue value: Data)
}
@available(macOS 27.0, *)
open class FSSetXattrResult : FSVolumeHandlerResult {
    public init?(freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSListXattrsResult : FSVolumeHandlerResult {
    public init?(xattrNames: [FSFileName])
}
@available(macOS 27.0, *)
open class FSReadFileResult : FSVolumeHandlerResult {
    public init?(bytesRead actuallyRead: Int, itemAttributes attributes: FSItem.Attributes)
}
@available(macOS 27.0, *)
open class FSWriteFileResult : FSVolumeHandlerResult {
    public init?(bytesWritten actuallyWritten: Int, itemAttributes attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSCheckAccessResult : FSVolumeHandlerResult {
    public init?(accessAllowed: Bool)
}
@available(macOS 27.0, *)
open class FSVolumeRenameResult : FSVolumeHandlerResult {
    public init?(newName: FSFileName)
}
@available(macOS 27.0, *)
open class FSPreallocateResult : FSVolumeHandlerResult {
    public init?(bytesAllocated: Int, itemAttributes attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSDeactivateItemResult : FSVolumeHandlerResult {
    public init?(freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSSeekRegionResult : FSVolumeHandlerResult {
    public init(returnedOffset: off_t)
}
@available(macOS 27.0, *)
open class FSBlockmapResult : FSVolumeHandlerResult {
    public init?(freeSpace: FSFreeSpace?)
}
@available(macOS 27.0, *)
open class FSCompleteIOResult : FSVolumeHandlerResult {
    public init?(itemAttributes attributes: FSItem.Attributes)
}
@available(macOS 27.0, *)
open class FSCreateFileKOIOResult : FSCreateItemResult {
}
@available(macOS 27.0, *)
open class FSLookupItemKOIOResult : FSLookupItemResult {
}
@available(macOS 27.0, *)
open class FSPreallocateKOIOResult : FSPreallocateResult {
}
@available(macOS 27.0, *)
open class FSOpenItemResult : FSVolumeHandlerResult {
    public init(grantedCoherency: FSVolume.KernelCacheCoherencyType)
}
@available(macOS 27.0, *)
open class FSUpgradeItemResult : FSVolumeHandlerResult {
    public init(grantedCoherency: FSVolume.KernelCacheCoherencyType)
}
@available(macOS 15.4, *)
public protocol UnaryFileSystemExtension : AppExtension {
    associatedtype FileSystem : FSUnaryFileSystem, FSUnaryFileSystemOperations
    var fileSystem: Self.FileSystem { get }
}
@available(macOS 15.4, *)
extension UnaryFileSystemExtension {
    public var configuration: some AppExtensionConfiguration { get }
}
