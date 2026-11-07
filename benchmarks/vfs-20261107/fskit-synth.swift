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

/// An interface for apps and daemons to interact with FSKit.
///
/// FSClient is the primary management interface for FSKit.
/// Use this class to discover FSKit extensions installed on the system, including your own.
///
/// > Important: Don't subclass `FSClient`.
@available(macOS 15.4, *)
open class FSClient : NSObject {

    /// The shared instance of the FSKit client class.
    open class var shared: FSClient { get }

    /// Asynchronously retrieves an list of installed file system modules.
    ///
    /// In Swift, you can either call this method and pass a completion handler closure, or get the value of the `installedExtensions` property with the `async` keyword.
    ///
    /// - Parameter completionHandler: A block or closure that executes when FSKit finishes its fetch process. If the fetch succeeds, the first parameter contains an array of ``FSModuleIdentity`` instances that identify installed modules. If the fetch fails, the second parameter contains an error detailing the failure.
    @available(macOS 15.4, *)
    open func fetchInstalledExtensions(completionHandler: @escaping @Sendable ([FSModuleIdentity]?, (any Error)?) -> Void)

    /// Asynchronously retrieves an list of installed file system modules.
    ///
    /// In Swift, you can either call this method and pass a completion handler closure, or get the value of the `installedExtensions` property with the `async` keyword.
    ///
    /// - Parameter completionHandler: A block or closure that executes when FSKit finishes its fetch process. If the fetch succeeds, the first parameter contains an array of ``FSModuleIdentity`` instances that identify installed modules. If the fetch fails, the second parameter contains an error detailing the failure.
    @available(macOS 15.4, *)
    open var installedExtensions: [FSModuleIdentity] { get async throws }

    /// Asynchronously mounts a single volume file system with a given resource.
    ///
    /// This method allows a client with the `com.apple.developer.fskit.mount` entitlement to mount a single-volume file system.
    /// Calling this method performs the complete workflow of resource loading, volume activation, mount point creation, and actual mounting.
    /// The system mounts the volume within the `/Volumes/` directory.
    ///
    /// The caller can only mount modules that are visible to them.
    ///
    /// - Parameters:
    ///   - resource: The resource to mount.
    ///   - bundleID: The bundle identifier of the file system extension.
    ///   - options: An array of strings containing the `mount_XXX` mount options
    ///   - completionHandler: A block or closure to indicate success or failure. If mount fails, the first parameter is nil and second parameter contains an error. If mount succeeds, the first parameter contains the URL of the mount path, and second parameter is `nil`.
    @available(macOS 27.0, *)
    open func mountSingleVolume(resource: FSResource, bundleID: String, options: [String], completionHandler: @escaping @Sendable (URL?, (any Error)?) -> Void)

    /// Asynchronously mounts a single volume file system with a given resource.
    ///
    /// This method allows a client with the `com.apple.developer.fskit.mount` entitlement to mount a single-volume file system.
    /// Calling this method performs the complete workflow of resource loading, volume activation, mount point creation, and actual mounting.
    /// The system mounts the volume within the `/Volumes/` directory.
    ///
    /// The caller can only mount modules that are visible to them.
    ///
    /// - Parameters:
    ///   - resource: The resource to mount.
    ///   - bundleID: The bundle identifier of the file system extension.
    ///   - options: An array of strings containing the `mount_XXX` mount options
    ///   - completionHandler: A block or closure to indicate success or failure. If mount fails, the first parameter is nil and second parameter contains an error. If mount succeeds, the first parameter contains the URL of the mount path, and second parameter is `nil`.
    @available(macOS 27.0, *)
    open func mountSingleVolume(resource: FSResource, bundleID: String, options: [String]) async throws -> URL

    /// Opens the File System Extensions settings in System Settings.
    ///
    /// Calling this method allows someone using your app to navigate to the File System Extensions pane in System Settings.
    /// From this pane, they can view, enable, and disable file system extensions.
    ///
    /// - Returns: `true` (Swift) or `YES` (Obj-C) if the settings were successfully opened; otherwise, `false` (Swift) or `NO`(Obj-C).
    @available(macOS 27.0, *)
    open func openFileSystemExtensionsSettings() -> Bool
}

/// An enumeration of container state values.
///
/// This enumeration represents values for a container's state engine.
/// Containers start in the ``notReady`` state.
@available(macOS 15.4, *)
public enum FSContainerState : Int, @unchecked Sendable {

    /// The container isn't ready.
    case notReady = 0

    /// The container is blocked from transitioning from the not-ready state to the ready state by a potentially-recoverable error.
    ///
    /// This state implies that the error has a resolution that would allow the container to become ready, such as correcting an incorrect password.
    case blocked = 1

    /// The container is ready, but inactive.
    case ready = 2

    /// The container is active, and one or more volumes are active.
    case active = 3
}

/// A type that represents a container's status.
///
/// This type contains two properties:
///
/// * The ``state`` value that indicates the state of the container, such as ``FSContainerState/ready`` or ``FSContainerState/blocked``.
/// * The ``status`` is an error (optional in Swift, nullable in Objective-C) that provides further information about the state, such as why the container is blocked.
///
/// Examples of statuses that require intervention include errors that indicate the container isn't ready (POSIX `EAGAIN` or `ENOTCONN`), the container needs authentication (`ENEEDAUTH`), or that authentication failed (`EAUTH`).
/// The status can also be an informative error, such as the FSKit error ``FSError/Code/statusOperationInProgress``.
///
open class FSContainerStatus : NSObject, NSCopying {

    /// A value that represents the container state, such as ready, active, or blocked.
    open var state: FSContainerState { get }

    /// An optional error that provides further information about the state.
    open var status: (any Error)? { get }

    /// A status that represents an active container with no error.
    ///
    /// This value is a ``FSContainerStatus`` with a ``state`` that is ``active``, and has a ``status`` that is `nil`.
    open class var active: FSContainerStatus { get }

    /// Returns a active container status instance with the provided error status.
    ///
    /// - Parameter errorStatus: The error status, if any, for the new instance.
    open class func active(status errorStatus: any Error) -> Self

    /// Returns a blocked container status instance with the provided error status.
    ///
    /// - Parameter errorStatus: The error status, if any, for the new instance.
    open class func blocked(status errorStatus: any Error) -> Self

    /// Returns a not-ready container status instance with the provided error status.
    ///
    /// - Parameter errorStatus: The error status, if any, for the new instance.
    open class func notReady(status errorStatus: any Error) -> Self

    /// A status that represents a ready container with no error.
    ///
    /// This value is a ``FSContainerStatus`` with a ``state`` that is ``ready``, and a ``status`` that is `nil`.
    open class var ready: FSContainerStatus { get }

    /// Returns a ready container status instance with the provided error status.
    ///
    /// - Parameter errorStatus: The error status, if any, for the new instance.
    open class func ready(status errorStatus: any Error) -> Self
}

/// A type that identifies a container.
///
/// The identifier is either a UUID or a UUID with additional differentiating bytes.
/// Some network protocols evaluate access based on a user ID when connecting.
/// In this situation, when a file server receives multiple client connections with different user IDs, the server provides different file hierarchies to each.
/// For such systems, represent the container identifier as the UUID associated with the server, followed by four or eight bytes to differentiate connections.
///
/// > Important: Don't subclass this class.
@available(macOS 15.4, *)
open class FSContainerIdentifier : FSEntityIdentifier {

    /// The volume identifier associated with the container.
    ///
    /// For unary file systems, the volume identifier is the same as the container identifier.
    @NSCopying open var volumeIdentifier: FSVolume.Identifier { get }
}

/// A context object that provides information about the initiator of a file system operation.
///
/// This object contains the user ID and group ID of the process that initiated the file system operation, allowing file systems to make authorization decisions based on the caller's identity.
/// Use this context in handler methods that provide the context as a parameter.
@available(macOS 27.0, *)
open class FSContext : NSObject {

    /// The caller's real user ID.
    open var realUserID: Int { get }

    /// The caller's effective user ID.
    open var effectiveUserID: Int { get }

    /// The caller's real group ID.
    open var realGroupID: Int { get }

    /// The caller's effective group ID.
    open var effectiveGroupID: Int { get }
}

/// A base type that identifies containers and volumes.
///
/// An ``FSEntityIdentifier`` is a UUID to identify a container or volume, optionally with eight bytes of qualifying (differentiating) data.
/// You use the qualifiers in cases in which a file server can receive multiple connections from the same client, which differ by user credentials.
/// In this case, the identifier for each client is the server's base UUID, and a unique qualifier that differs by client.
///
/// > Important: Don't subclass this class.
@available(macOS 15.4, *)
open class FSEntityIdentifier : NSObject, NSCopying, NSSecureCoding {

    /// Creates an entity identifier with a random UUID.
    public init()

    /// Creates an entity identifier with the given UUID.
    ///
    /// - Parameter uuid: The UUID to use for this identifier.
    public init(uuid: UUID)

    /// Creates an entity identifier with the given UUID and qualifier data as a 64-bit unsigned integer.
    /// - Parameters:
    ///   - uuid: The UUID to use for this identifier.
    ///   - qualifier: The data to distinguish entities that otherwise share the same UUID.
    public init(uuid: UUID, qualifier: UInt64)

    /// Creates an entity identifier with the given UUID and qualifier data.
    ///
    /// - Parameters:
    ///   - uuid: The UUID to use for this identifier.
    ///   - qualifierData: The data to distinguish entities that otherwise share the same UUID.
    ///
    /// - Warning: This initializer is annotated as returning a non-optional value but silently
    ///   returns `nil` when `qualifierData` is not exactly eight bytes, which can surface as a
    ///   null value in a non-optional Swift variable. Use ``initWithUUID:qualifierData:``
    ///   instead, which is explicitly failable.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public init(uuid: UUID, data qualifierData: Data)

    /// Creates an entity identifier with the given UUID and qualifier data.
    ///
    /// - Parameters:
    ///   - uuid: The UUID to use for this identifier.
    ///   - qualifierData: The data to distinguish entities that otherwise share the same UUID.
    ///     Must be exactly eight bytes; any other length causes this initializer to return `nil`.
    /// - Returns: A new identifier, or `nil` if `qualifierData` is not exactly eight bytes long.
    @available(macOS 27.0, *)
    public init?(uuid: UUID, qualifierData: Data)

    /// A UUID to uniquely identify this entity.
    open var uuid: UUID

    /// An optional piece of data to distinguish entities that otherwise share the same UUID.
    open var qualifier: Data?
}

@available(macOS 15.4, *)
extension FSEntityIdentifier : Identifiable, @unchecked Sendable {

    /// The stable identity of the entity associated with this instance.
    @available(macOS 15.4, *)
    public var id: String { get }

    /// A type representing the stable identity of the entity associated with
    /// an instance.
    @available(macOS 15.4, *)
    public typealias ID = String
}

/// The name of a file, expressed as a data buffer.
///
/// `FSFileName` is the class that carries filenames from the kernel to `FSModule` instances, and carries names back to the kernel as part of directory enumeration.
///
/// A filename is usually a valid UTF-8 sequence, but can be an arbitrary byte sequence that doesn't conform to that format.
/// As a result, the ``data`` property always contains a value, but the ``string`` property may be empty.
/// An `FSModule` can receive an `FSFileName` that isn't valid UTF-8 in two cases:
/// 1. A program passes erroneous data to a system call. The `FSModule` treats this situation as an error.
/// 2. An `FSModule` lacks the character encoding used for a file name.
/// This situation occurs because some file system formats consider a filename to be an arbitrary "bag of bytes," and leave character encoding up to the operating system.
/// Without encoding information, the `FSModule` can only pass back the names it finds on disk.
/// In this case, the behavior of upper layers such as <doc://com.apple.documentation/documentation/Foundation/FileManager> is unspecified.
/// However, the `FSModule` must support looking up such names and using them as the source name of rename operations.
/// The `FSModule` must also be able to support filenames that are derivatives of filenames returned from directory enumeration.
/// Derivative filenames include Apple Double filenames (`"._Name"`), and editor backup filenames.
///
/// > Important: Don't subclass this class.
@available(macOS 15.4, *)
open class FSFileName : NSObject, NSSecureCoding, NSCopying {

    /// The byte sequence of the filename, as a data object.
    ///
    /// This property always provides a value.
    @available(macOS 15.4, *)
    open var data: Data { get }

    /// The filename, represented as a Unicode string.
    ///
    /// If the value of the filename's ``FSFileName/data`` is not a valid UTF-8 byte sequence, this property is empty.
    @available(macOS 15.4, *)
    open var string: String? { get }

    /// The filename, represented as a potentially lossy conversion to a string.
    ///
    /// The exact details of the string conversion may change in the future.
    @available(macOS 15.4, *)
    open var debugDescription: String { get }

    /// Creates a filename by copying a character sequence data object.
    ///  
    /// This initializer copies up to `name.length` characters of the sequence pointed to by `bytes`.
    ///  
    /// - Parameter name: The data object containing the character sequence to use for the filename. The sequence terminates if a `NUL` character exists prior to `name.length`.
    @available(macOS 15.4, *)
    public convenience init(data name: Data)

    /// Creates a filename by copying a character sequence from a string instance.
    ///
    /// This initializer copies the UTF-8 representation of the characters in `string`.
    /// If `string` contains a `NUL` character, the sequence terminates.
    ///
    /// - Parameter name: The string containing the character sequence to use for the filename.
    @available(macOS 15.4, *)
    public convenience init(string name: String)
}

@available(macOS 15.4, *)
@available(iOS, unavailable)
@available(watchOS, unavailable)
@available(tvOS, unavailable)
@available(visionOS, unavailable)
extension FSFileName {

    @available(macOS 15.4, *)
    public convenience init(cString name: UnsafeBufferPointer<CChar>)

    @available(macOS 15.4, *)
    public convenience init(bytes name: UnsafeBufferPointer<CChar>)
}

/// A protocol containing functionality supplied by FSKit to file system implementations.
///
/// Both ``FSFileSystem`` and ``FSUnaryFileSystem`` conform to this protocol.
@available(macOS 15.4, *)
public protocol FSFileSystemBase : NSObjectProtocol {

    /// The status of the file system container, indicating its readiness and activity.
    ///
    /// A file system container starts in the ``FSContainerState/notReady`` state, and then transitions to the other values of the ``FSContainerState`` enumeration.
    /// The following diagram illustrates the possible state transitions.
    ///
    /// ![A flow diagram of four possible container states. The initial state, notReady, can transition to ready or blocked. The ready state can transition back to not ready, or to blocked or active. The blocked state can transition to ready or not ready. The active state can transition back to ready or to not ready.](fs-file-system-base)
    ///
    /// Your file system implementation updates this property as it changes state.
    /// Many events and operations may trigger a state transition, and some transitions depend on a specific file system's design.
    ///
    /// When using ``FSBlockDeviceResource``, implement the following common state transitions:
    ///
    /// * Calling `loadResource` transitions the state out of ``FSContainerState/notReady``. For all block device file systems, this operation changes the state to either ``FSContainerState/ready`` or ``FSContainerState/blocked``.
    /// * Calling `unloadResource` transitions to the ``FSContainerState/notReady`` state, as does device termination.
    /// * Transitioning from ``FSContainerState/blocked`` to ``FSContainerState/ready`` occurs as a result of resolving the underlying block favorably.
    /// * Transitioning from ``FSContainerState/ready`` to ``FSContainerState/blocked`` is unusal, but valid.
    /// * Transitioning between ``FSContainerState/ready`` and ``FSContainerState/active`` can result from maintenance operations such as ``FSManageableResourceMaintenanceOperations/startCheckWithTask:options:error:``. For a ``FSUnaryFileSystem``, this transition can also occur when activating or deactivating the container's single volume.
    ///
    @available(macOS 15.4, *)
    @NSCopying var containerStatus: FSContainerStatus { get set }

    /// Wipes existing file systems on the specified resource.
    ///
    /// This method wraps the `wipefs` functionality from `libutil`.
    /// For more information, see the `man` page for `wipefs`.
    ///
    /// - Parameters:
    ///   - resource: The ``FSBlockDeviceResource`` to wipe.
    ///   - completion: A block or closure that executes after the wipe operation completes. The completion handler receives a single parameter indicating any error that occurs during the operation. If the value is `nil`, the wipe operation succeeded.
    @available(macOS 15.4, *)
    func wipe(_ resource: FSBlockDeviceResource, completionHandler completion: @escaping @Sendable ((any Error)?) -> Void)

    /// Wipes existing file systems on the specified resource.
    ///
    /// This method wraps the `wipefs` functionality from `libutil`.
    /// For more information, see the `man` page for `wipefs`.
    ///
    /// - Parameters:
    ///   - resource: The ``FSBlockDeviceResource`` to wipe.
    ///   - completion: A block or closure that executes after the wipe operation completes. The completion handler receives a single parameter indicating any error that occurs during the operation. If the value is `nil`, the wipe operation succeeded.
    @available(macOS 15.4, *)
    func wipe(_ resource: FSBlockDeviceResource) async throws
}

/// A free space object that pairs free space values with atomic sequence numbers.
@available(macOS 27.0, *)
open class FSFreeSpace : NSObject {

    /// Creates an unpopulated free space instance.
    public init()

    /// Populates this instance with the given free space value and atomically assigns a sequence number.
    ///
    /// > Important: FSKit uses internal sequencing technology within this method to determine the most up-to-date free space of the volume. To ensure correctness, call this method within an isolation context covering the volume's free space.
    open func populate(bytes freeSpaceBytes: UInt64)

    /// A sentinel instance that indicates no free space update occurred.
    open class var noUpdate: FSFreeSpace { get }
}

/// A distinct object in a file hierarchy, such as a file, directory, symlink, socket, and more.
///
/// An `FSItem` is a mostly opaque object, which your file system implementation defines as needed.
///
/// The ``FSItemAttributes`` class defines nonatomic properties to support `FSItem` instances.
/// An ``FSItemAttributes`` instance contains a snapshot of the attributes of an `FSItem` at one point in time.
/// The ``FSItemAttributes`` properties have no explicit thread safety provisions, since the operations that either get or set these properties enforce thread safety.
///
/// You test an attribute's validity with the the method ``FSItem/Attributes/isValid(_:)``.
/// If the value is `true` (Swift) or `YES` (Objective-C), it's safe to use the attribute.
///
/// Methods that get or set an item's attribute use ``FSItemGetAttributesRequest`` or ``FSItemSetAttributesRequest``, respectively.
/// Both are subclasses of ``FSItemAttributes``.
/// An ``FSItemGetAttributesRequest`` contains a ``FSItemGetAttributesRequest/wantedAttributes`` property to indicate the attributes a file system provides for the request.
/// Similarly, ``FSItemSetAttributesRequest`` uses the property ``FSItemSetAttributesRequest/consumedAttributes`` for a file system to signal back which attributes it successfully used.
///
/// `FSItem` is the FSKit equivelant of a vnode in the kernel.
/// For every FSKit vnode in the kernel, the `FSModule` hosting the volume has an instantiated `FSItem`.
@available(macOS 15.4, *)
open class FSItem : NSObject {

    /// Reclaims the item by executing the given block, if conditions allow.
    ///
    /// Invoke this method in your implementation of the ``FSVolume/Handler/reclaimItem(_:replyHandler:)`` operation.
    ///
    /// FSKit internally maintains a count of how many times it returns each ``FSItem`` to the kernel, via either a creation operation or a lookup. The kernel file system also maintains a count of how many times a create or a lookup operation returned a vnode. When the kernel reclaims the vnode associated with an FSItem, the FSItem is only eligible for reclaiming when both the kernel and user space counts agree.
    /// This mechanism addresses a potential race condition in which concurrent reclaim and lookup operations might lead to a lookup returning a deallocated ``FSItem``, and as a result, induce undefined behavior.
    /// File systems that don't invoke this method during reclaim are exposed to this race condition.
    ///
    /// > Important: The caller must invoke this method within a synchronization context that ensures the ``FSItem`` isn't concurrently returned by lookup operations.
    ///
    /// - Parameters:
    ///    - reclaimBlock The closure or block to run if the reclaim can proceed. In your closure or block, include all required cleanup operations for reclaiming this item, excluding only the final teardown of the ``FSItem`` instance itself.
    ///
    /// - Returns: `true` (Swift) or YES (Obj-C)  if the reclaim block ran; otherwise, `false` (Swift) or `NO` (Obj-C). When your ``FSVolume/Handler/reclaimItem(_:replyHandler:)`` implementation receives a `false`/`NO` return value, call `replyHandler(nil)`.
    ///
    /// Example Usage:
    ///
    ///@TabNavigator {
    ///    @Tab("Swift") {
    ///```swift
    /// func reclaimItem(_ item: FSItem,
    ///                  replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
    ///     var reclaimError: NSError? = nil // To be set during the reclaim block in case of an error.
    ///
    ///     // *** CRITICAL SECTION BEGINS HERE ***
    ///     // (A synchronization context that ensures the FSItem isn't concurrently returned by lookup operations)
    ///
    ///     // Calling `tryReclaim(_:)` with the cleanup logic within the passed block
    ///     let wasReclaimed = item.tryReclaim( {
    ///         // Closure includes all required cleanup operations for reclaiming this item.
    ///         // Sets `reclaimError` in case of an error during the cleanup phase.
    ///     } )
    ///
    ///     // *** CRITICAL SECTION ENDS HERE ***
    ///
    ///     if (wasReclaimed) {
    ///         // Clean up the FSItem if special teardown is needed.
    ///         reply(reclaimError)
    ///     } else {
    ///         // Do nothing; the FSItem wasn't reclaimed, so it's not yet time to run cleanup.
    ///         reply(nil)
    ///     }
    ///}
    ///```
    ///    }
    ///    @Tab("Objective-C") {
    /// ```objc
    /// - (void)reclaimItem:(FSItem *)item
    ///     replyHandler:(void(^)(NSError * _Nullable error))reply
    /// {
    ///     __block NSError *reclaimError = nil; // To be set during the reclaim block in case of an error.
    ///
    ///     // *** CRITICAL SECTION BEGINS HERE ***
    ///     // (A synchronization context that ensures the FSItem isn't concurrently returned by lookup operations)
    ///
    ///     // Calling `tryReclaimWithBlock:` with the cleanup logic within the passed block
    ///     BOOL wasReclaimed = [item tryReclaimWithBlock:^{
    ///         // Includes all required cleanup operations for reclaiming this item.
    ///         // Sets `reclaimError` in case of an error during the cleanup phase
    ///     }];
    ///
    ///     // *** CRITICAL SECTION ENDS HERE ***
    ///
    ///     if (wasReclaimed) {
    ///         // Clean up the FSItem if special teardown is needed.
    ///         reply(reclaimError);
    ///     } else {
    ///         // Do nothing; the FSItem wasn't reclaimed, so it's not yet time to run cleanup.
    ///         reply(nil);
    ///     }
    /// }
    /// ```
    ///    }
    /// }
    @available(macOS 27.0, *)
    open func tryReclaim(_ reclaimBlock: @escaping () -> Void) -> Bool
}

extension FSItem {

    /// Attributes of an item, such as size, creation and modification times, and user and group identifiers.
    ///
    /// > Important: FSKit internally maintains a unique sequence number for each set of attributes to determine the most up-to-date set of attributes of a given ``FSItem``.
    /// To prevent an older set of attributes from having a newer sequence number, all attributes of a given ``FSItemAttributes`` object should be populated within the same synchronization context.
    /// This synchronization context should prevent any other thread from populating another ``FSItemAttributes`` object of the given ``FSItem``.
    @available(macOS 15.4, *)
    open class Attributes : NSObject, NSSecureCoding {

        /// Marks all attributes inactive.
        open func invalidateAllProperties()

        /// The user identifier.
        open var uid: UInt32

        /// The group identifier.
        open var gid: UInt32

        /// The mode of the item.
        ///
        /// The mode is often used for `setuid`, `setgid`, and `sticky` bits.
        open var mode: UInt32

        /// The item type, such as a regular file, directory, or symbolic link.
        open var type: FSItem.ItemType

        /// The number of hard links to the item.
        open var linkCount: UInt32

        /// The item's behavior flags.
        ///
        /// See `st_flags` in `stat.h` for flag definitions.
        open var flags: UInt32

        /// The item's size.
        open var size: UInt64

        /// The item's allocated size.
        open var allocSize: UInt64

        /// The item's file identifier.
        open var fileID: FSItem.Identifier

        /// The identifier of the item's parent.
        open var parentID: FSItem.Identifier

        /// A Boolean value that indicates whether the item supports a limited set of extended attributes.
        open var supportsLimitedXAttrs: Bool

        /// A Boolean value that indicates whether the file system overrides the per-volume settings for kernel offloaded I/O for a specific file.
        /// FSKit reads this value the first time the module reports attributes for a given file; It then sets the file's kernel-offloaded-I/O routing accordingly.
        /// The routing stays in effect for the item's lifetime; later changes are ignored.
        ///
        /// This property has no meaning if the volume doesn't conform to ``FSVolumeKernelOffloadedIOOperations`` or ``FSVolume/KernelOffloadedIOHandler``.
        open var inhibitKernelOffloadedIO: Bool

        /// The item's last-modified time.
        ///
        /// This property represents `mtime`, the last time the item's contents changed.
        open var modifyTime: timespec

        /// The item's added time.
        ///
        /// This property represents the time the file system added the item to its parent directory.
        open var addedTime: timespec

        /// The item's last-changed time.
        ///
        /// This property represents `ctime`, the last time the item's metadata changed.
        open var changeTime: timespec

        /// The item's last-accessed time.
        open var accessTime: timespec

        /// The item's creation time.
        open var birthTime: timespec

        /// The item's last-backup time.
        open var backupTime: timespec

        /// Returns a Boolean value that indicates whether the attribute is valid.
        ///
        /// If the value returned by this method is `YES` (Objective-C) or `true` (Swift), a caller can safely use the given attribute.
        open func isValid(_ attribute: FSItem.Attribute) -> Bool
    }

    /// A request to set attributes on an item.
    ///
    /// Methods that take attributes use this type to receive attribute values and to indicate which attributes they support.
    /// The various members of the parent type, ``FSItemAttributes``, contain the values of the attributes to set.
    ///
    /// Modify the ``consumedAttributes`` property to indicate which attributes your file system successfully used.
    /// FSKit calls the ``wasAttributeConsumed(_:)`` method to determine whether the file system successfully used a given attribute.
    /// Only set the attributes that your file system supports.
    @available(macOS 15.4, *)
    open class SetAttributesRequest : FSItem.Attributes {

        /// The attributes successfully used by the file system.
        ///
        /// This property is a bit field in Objective-C and an <doc://com.apple.documentation/documentation/Swift/OptionSet> in Swift.
        open var consumedAttributes: FSItem.Attribute

        /// A method that indicates whether the file system used the given attribute.
        ///
        /// - Parameter attribute: The ``FSItemAttribute`` to check.
        open func wasAttributeConsumed(_ attribute: FSItem.Attribute) -> Bool
    }

    /// A request to get attributes from an item.
    ///
    /// Methods that retrieve attributes use this type and inspect the ``wantedAttributes`` property to determine which attributes to provide. FSKit calls the ``isAttributeWanted(_:)`` method to determine whether the request requires a given attribute.
    @available(macOS 15.4, *)
    open class GetAttributesRequest : NSObject, NSSecureCoding {

        /// The attributes requested by the request.
        ///
        /// This property is a bit field in Objective-C and an <doc://com.apple.documentation/documentation/Swift/OptionSet> in Swift.
        open var wantedAttributes: FSItem.Attribute

        /// A method that indicates whether the request wants given attribute.
        ///
        /// - Parameter attribute: The ``FSItemAttribute`` to check.
        open func isAttributeWanted(_ attribute: FSItem.Attribute) -> Bool
    }

    /// A value that indicates a set of item attributes to get or set.
    ///
    /// This type is an option set in Swift.
    /// In Objective-C, you use the cases of this enumeration to create a bit field.
    @available(macOS 15.4, *)
    public struct Attribute : OptionSet, @unchecked Sendable {

        public init(rawValue: Int)

        /// The type attribute.
        public static var type: FSItem.Attribute { get }

        /// The mode attribute.
        public static var mode: FSItem.Attribute { get }

        /// The link count attribute.
        public static var linkCount: FSItem.Attribute { get }

        /// The user ID (uid) attribute.
        public static var uid: FSItem.Attribute { get }

        /// The group ID (gid) attribute.
        public static var gid: FSItem.Attribute { get }

        /// The flags attribute.
        public static var flags: FSItem.Attribute { get }

        /// The size attribute.
        public static var size: FSItem.Attribute { get }

        /// The allocated size attribute.
        public static var allocSize: FSItem.Attribute { get }

        /// The file ID attribute.
        public static var fileID: FSItem.Attribute { get }

        /// The parent ID attribute.
        public static var parentID: FSItem.Attribute { get }

        /// The last-accessed time attribute.
        public static var accessTime: FSItem.Attribute { get }

        /// The last-modified time attribute.
        public static var modifyTime: FSItem.Attribute { get }

        /// The last-changed time attribute.
        public static var changeTime: FSItem.Attribute { get }

        /// The creation time attribute.
        public static var birthTime: FSItem.Attribute { get }

        /// The backup time attribute.
        public static var backupTime: FSItem.Attribute { get }

        /// The time added attribute.
        public static var addedTime: FSItem.Attribute { get }

        /// The supports limited extended attributes attribute.
        public static var supportsLimitedXAttrs: FSItem.Attribute { get }

        /// The inhibit kernel offloaded I/O attribute.
        public static var inhibitKernelOffloadedIO: FSItem.Attribute { get }
    }

    /// An enumeration of item types, such as file, directory, or symbolic link.
    @available(macOS 15.4, *)
    public enum ItemType : Int, @unchecked Sendable {

        /// The item type of an unknown item.
        case unknown = 0

        /// The item type of a regular file.
        case file = 1

        /// The item type of a directory.
        case directory = 2

        /// The item type of a symbolic link.
        case symlink = 3

        /// The item type of a first-in/first-out named pipe.
        case fifo = 4

        /// The item type of a character device.
        case charDevice = 5

        /// The item type of a block device.
        case blockDevice = 6

        /// The item type of a socket.
        case socket = 7
    }

    /// The unique identifier for an item.
    ///
    /// Use this type when packing items for an enumeration in ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)``.
    /// Either provide a unique identifier like an inode number, or one of the special enumeration cases this type defines, like ``FSItem/Identifier/rootDirectory``.
    @available(macOS 15.4, *)
    public enum Identifier : UInt64, @unchecked Sendable {

        /// The identifier for an invalid item.
        case invalid = 0

        /// The identifier for an item that serves as the parent of the root directory.
        case parentOfRoot = 1

        /// The item identifier for the root directory.
        case rootDirectory = 2
    }
}

/// An error domain for FSKit errors.
///
/// See <doc://com.apple.documentation/documentation/Foundation/NSError> for more information on error domains.
@available(macOS 15.4, *)
public let FSKitErrorDomain: String

public struct FSError : CustomNSError, Hashable, Error {

    public init(_nsError: NSError)

    public static var errorDomain: String { get }

    /// A code that indicates a specific FSKit error.
    public enum Code : Int, @unchecked Sendable, Equatable {

        /// A code that indicates a specific FSKit error.
        public typealias _ErrorType = FSError

        /// The module failed to load.
        case moduleLoadFailed = 4500

        /// FSKit didn't recognize the resource, and probing failed to find a match.
        case resourceUnrecognized = 4501

        /// The resource is damaged.
        ///
        /// This error indicates the resource needs a repair operation, or that a repair operation failed.
        /// > Note: The status in this error applies to the resource. A failing repair operation reports a more specific error status.
        case resourceDamaged = 4502

        /// FSKit recognizes the resource, but the resource isn't usable.
        ///
        /// For example, this error occurs when a resource uses a file system's internal feature flags.
        /// If the only modules that support the file system don't support those feature flags, this code indicates an unusable resource.
        /// The error tells the person using the module why the resource isn't usable.
        case resourceUnusable = 4503

        /// An operation is in progress.
        case statusOperationInProgress = 4504

        /// An operation is paused.
        case statusOperationPaused = 4505

        /// While enumerating a directory, the given cookie didn't resolve to a valid directory entry.
        case invalidDirectoryCookie = 4506
    }

    /// The module failed to load.
    public static var moduleLoadFailed: FSError.Code { get }

    /// FSKit didn't recognize the resource, and probing failed to find a match.
    public static var resourceUnrecognized: FSError.Code { get }

    /// The resource is damaged.
    ///
    /// This error indicates the resource needs a repair operation, or that a repair operation failed.
    /// > Note: The status in this error applies to the resource. A failing repair operation reports a more specific error status.
    public static var resourceDamaged: FSError.Code { get }

    /// FSKit recognizes the resource, but the resource isn't usable.
    ///
    /// For example, this error occurs when a resource uses a file system's internal feature flags.
    /// If the only modules that support the file system don't support those feature flags, this code indicates an unusable resource.
    /// The error tells the person using the module why the resource isn't usable.
    public static var resourceUnusable: FSError.Code { get }

    /// An operation is in progress.
    public static var statusOperationInProgress: FSError.Code { get }

    /// An operation is paused.
    public static var statusOperationPaused: FSError.Code { get }

    /// While enumerating a directory, the given cookie didn't resolve to a valid directory entry.
    public static var invalidDirectoryCookie: FSError.Code { get }
}

@available(macOS 27.0, *)
public struct FSDataCacheError : CustomNSError, Hashable, Error {

    public init(_nsError: NSError)

    public static var errorDomain: String { get }

    /// Error codes specific to data cache operations.
    @available(macOS 27.0, *)
    public enum Code : Int, @unchecked Sendable, Equatable {

        /// Error codes specific to data cache operations.
        public typealias _ErrorType = FSDataCacheError

        /// The requested cache mode and coherency type combination is invalid.
        case errorInvalidCacheModeCoherency = 4510

        /// The cache transition is not allowed.
        ///
        /// This error occurs when attempting an invalid transition, such as using an upgrade
        /// method for a downgrade operation, or vice versa.
        case errorInvalidCacheTransition = 4511

        /// Failed to flush dirty cached data to storage.
        case errorCacheFlushFailed = 4512

        /// Failed to invalidate (clear) cached data.
        case errorCacheInvalidationFailed = 4513

        /// A conflicting cache operation is in progress.
        ///
        /// This error occurs when multiple cache operations on the same item conflict,
        /// such as attempting to change cache mode while I/O is active.
        case errorCacheOperationConflict = 4514
    }

    /// The requested cache mode and coherency type combination is invalid.
    public static var errorInvalidCacheModeCoherency: FSDataCacheError.Code { get }

    /// The cache transition is not allowed.
    ///
    /// This error occurs when attempting an invalid transition, such as using an upgrade
    /// method for a downgrade operation, or vice versa.
    public static var errorInvalidCacheTransition: FSDataCacheError.Code { get }

    /// Failed to flush dirty cached data to storage.
    public static var errorCacheFlushFailed: FSDataCacheError.Code { get }

    /// Failed to invalidate (clear) cached data.
    public static var errorCacheInvalidationFailed: FSDataCacheError.Code { get }

    /// A conflicting cache operation is in progress.
    ///
    /// This error occurs when multiple cache operations on the same item conflict,
    /// such as attempting to change cache mode while I/O is active.
    public static var errorCacheOperationConflict: FSDataCacheError.Code { get }
}

/// Creates an error object for the given POSIX error code.
@available(macOS 15.4, *)
public func fs_errorForPOSIXError(_: Int32) -> any Error

/// Creates an error object for the given Mach error code.
@available(macOS 15.4, *)
public func fs_errorForMachError(_ errorCode: Int32) -> any Error

/// Creates an error object for the given Cocoa error code.
@available(macOS 15.4, *)
public func fs_errorForCocoaError(_ errorCode: Int32) -> any Error

/// An installed file system module.
@available(macOS 15.4, *)
open class FSModuleIdentity : NSObject {

    /// The module's bundle identifier.
    @available(macOS 15.4, *)
    open var bundleIdentifier: String { get }

    /// The module's URL.
    @available(macOS 15.4, *)
    open var url: URL { get }

    /// A Boolean value that indicates if the module is enabled.
    @available(macOS 15.4, *)
    open var isEnabled: Bool { get }
}

@available(macOS 15.4, *)
@available(iOS, unavailable)
@available(watchOS, unavailable)
@available(tvOS, unavailable)
@available(visionOS, unavailable)
extension FSModuleIdentity : Identifiable {

    /// The stable identity of the entity associated with this instance.
    @available(macOS 15.4, *)
    public var id: String { get }

    /// A type representing the stable identity of the entity associated with
    /// an instance.
    @available(macOS 15.4, *)
    @available(iOS, unavailable)
    @available(tvOS, unavailable)
    @available(watchOS, unavailable)
    @available(visionOS, unavailable)
    public typealias ID = String
}

/// A wrapper object for a data buffer.
///
/// This object provides a "zero-copy" buffer, for use when reading data from files.
/// By not requiring additional buffer copying, this object reduces the extension's memory footprint and improves performance.
/// The `FSMutableFileDataBuffer` behaves similarly to a `uio` in the kernel.
@available(macOS 15.4, *)
open class FSMutableFileDataBuffer : NSObject {

    /// The data length of the buffer.
    @available(macOS 15.4, *)
    open var length: Int { get }
}

@available(macOS 15.4, *)
extension FSMutableFileDataBuffer {

    /// Performs the given closure with an unsafe pointer to the underlying bytes of the data buffer.
    /// - Parameter body: The closure to perform with the pointer.
    @available(macOS 15.4, *)
    public func withUnsafeMutableBytes<R, E>(_ body: (UnsafeMutableRawBufferPointer) throws(E) -> R) throws(E) -> R where E : Error

    /// Return a MutableRawSpan to the underlying bytes of the data buffer.
    ///
    @available(macOS 27.0, *)
    public func createMutableRawSpan() throws -> MutableRawSpan
}

/// A type that represents the recognition and usability of a probed resource.
@available(macOS 15.4, *)
public enum FSMatchResult : Int, @unchecked Sendable {

    /// The probe doesn't recognize the resource.
    ///
    /// This match result is appropriate when the file system module determines that the resource uses a completely different format.
    case notRecognized = 0

    /// The probe recognizes the resource but can't use it.
    ///
    /// This match result is appropriate when the file system module identifies the resource's format but can't use it. For example, if the resource uses a newer version than the module supports, the module can name the resource but can't safely do anything with it.
    case recognized = 1

    /// The probe recognizes the resource and is ready to use it, but only in a limited capacity.
    ///
    /// This match result is appropriate when the file system module identifies the resource's format but also identifies incompatibilities. For example, if the module determines the resource uses new features that the module doesn't support, the module may only offer read-only access.
    case usableButLimited = 2

    /// The probe recognizes the resource and is ready to use it.
    case usable = 3
}

/// An abstract resource a file system uses to provide data for a volume.
///
/// `FSResource` is a base class to represent the various possible sources of data for a file system.
/// These range from dedicated storage devices like hard drives and flash storage to network connections, and beyond.
/// Subclasses define behavior specific to a given kind of resource, such as ``FSBlockDeviceResource-c.class`` for disk partition (IOMedia) file systems.
/// These file systems are typical disk file systems such as HFS, APFS, ExFAT, ext2fs, or NTFS.
///
/// A resource's type also determines its life cycle.
/// Resources based on block storage devices come into being when the system probes the media underlying the volumes and container.
/// Other kinds of resources, like those based on URLs, might have different life cycles.
/// For example, a resource based on a `file://` URL might initialize when a person uses the "Connect to server" command in the macOS Finder.
///
/// ### Proxying resources
///
/// Some resources, like ``FSBlockDeviceResource``, come in proxy and non-proxy variants.
/// This addresses the issue that opening an external device like `/dev/disk2s1` requires an entitlement.
/// Proxy resources allow unentitled clients of FSKit to describe which disk an ``FSBlockDeviceResource`` should represent.
/// This allows, for example, the `mount(8)` tool to mount FSKit file systems on block devices when run as root.
/// The tool uses a proxy when executing a command like `mount -t ffs /dev/disk2s1 /some/path`, which prevents leaking privileged resource access.
@available(macOS 15.4, *)
open class FSResource : NSObject, NSSecureCoding {

    /// A Boolean value that indicates whether the resource is revoked.
    ///
    /// If this is a proxy resource, the value of this property is always `true` (Swift) or `YES` (Objective-C).
    open var isRevoked: Bool { get }

    /// Creates a proxy object of this resource.
    ///
    /// If you create a proxy from a proxy resource, this method returns a copy of the proxy.
    open func makeProxy() -> Self

    /// Revokes the resource.
    ///
    /// This method works by stripping away any underlying privileges associated with the resource.
    /// This effectively disconnects this object from its underlying resource.
    open func revoke()
}

/// A range that describes contiguous metadata segments on disk.
///
/// This type represents a range that begins at `startOffset` and ends at `startOffset + segmentLength * segmentCount`.
/// Each segment in the range represents a single block in the resource's buffer cache.
///
/// For example, given an `FSMetadataRange` with the following properties:
///
/// * `startOffset = 0`
/// * `segmentLength = 512`
/// * `segmentCount = 8`
///
/// The range represents eight segments: from 0 to 511, then from 512 to 1023, and so on until a final segment of 3584 to 4095.
///
/// Ensure that each metadata segment represents a range that's already present in the resource's buffer cache.
/// Similarly, ensure that each segment's offset and length matches the offset and length of the corresponding block in the buffer cache.
open class FSMetadataRange : NSObject {

    /// The start offset of the range in bytes.
    ///
    /// Ensure this value is a multiple of the corresponding resource's ``FSBlockDeviceResource-c.class/blockSize``.
    open var startOffset: off_t { get }

    /// The segment length in bytes.
    ///
    /// Ensure this value is a multiple of the corresponding resource's ``FSBlockDeviceResource-c.class/blockSize``.
    open var segmentLength: UInt64 { get }

    /// The number of segments in the range.
    open var segmentCount: UInt64 { get }

    /// Initializes a metadata range with the given properties.
    ///
    /// - Parameters:
    ///   - startOffset: The start offset of the range in bytes. Ensure this value is a multiple of the corresponding resource's ``FSBlockDeviceResource-c.class/blockSize``.
    ///   - segmentLength: The segment length in bytes. Ensure this value is a multiple of the corresponding resource's ``FSBlockDeviceResource-c.class/blockSize``.
    ///   - segmentCount: The number of segments in the range.
    public init(offset startOffset: off_t, segmentLength: UInt64, segmentCount: UInt64)
}

/// A resource that represents a block storage disk partition.
///
/// A `FSBlockDeviceResource` can exist in either a proxied or nonproxied version.
///  Only the `fskitd` daemon creates "real" (nonproxied) instances of this class.
///  Client applications and daemons create proxy objects for requests, and `fskitd` opens the underlying device during the processing of the request.
///
///  This class wraps a file descriptor for a disk device or partition.
///  Its fundamental identifier is the BSD disk name (``bsdName``) for the underlying IOMedia object.
///  However, ``FSBlockDeviceResource-c.class`` doesn't expose the underlying file descriptor.
///  Instead, it provides accessor methods that can read from and write to the partition, either directly or using the kernel buffer cache.
///
///  When you use a `FSBlockDeviceResource`, your file system implementation also conforms to a maintenance operation protocol.
///  These protocols add support for checking, repairing, and optionally formatting file systems.
///  The system doesn't mount block device file systems until they pass a file system check.
///  For an ``FSUnaryFileSystem`` that uses `FSBlockDeviceResource`, conform to `FSManageableResourceMaintenanceOperations`.
///
/// > Important: Don't mix direct I/O operations (``read(into:startingAt:length:)`` and ``write(from:startingAt:length:)``) with metadata operations (``metadataRead(into:startingAt:length:)``, ``metadataWrite(from:startingAt:length:)``, and ``delayedMetadataWrite(from:startingAt:length:)``) on the same range. Direct I/O bypasses the buffer cache, so mixing the two may result in stale or inconsistent data.
@available(macOS 15.4, *)
open class FSBlockDeviceResource : FSResource {

    /// The device name of the resource.
    open var bsdName: String { get }

    /// A Boolean property that indicates whether the resource can write data to the device.
    open var isWritable: Bool { get }

    /// The logical block size, the size of data blocks used by the file system.
    ///
    /// This is equivalent to the `DKIOCGETBLOCKSIZE` device parameter.
    open var blockSize: UInt64 { get }

    /// The block count on this resource.
    open var blockCount: UInt64 { get }

    /// The sector size of the device.
    ///
    /// This is equivalent to the `DKIOCGETPHYSICALBLOCKSIZE` device parameter.
    open var physicalBlockSize: UInt64 { get }

    /// Synchronously flushes the resource's buffer cache.
    ///
    /// This method flushes data previously written with ``delayedMetadataWriteFrom:startingAt:length:error:`` to the resource.
    ///
    /// - Parameter error: On return, any error encountered while writing data, or `nil` if no error occurred.
    ///
    /// - Returns: A Boolean value indicating whether the metadata flush succeeded.
    @available(macOS 15.4, *)
    open func metadataFlush() throws

    /// Asynchronously flushes the resource's buffer cache.
    ///
    /// This method schedules a flush of data previously written with ``delayedMetadataWriteFrom:startingAt:length:error:`` to the resource and returns immediately without blocking.
    /// This method doesn't wait to check the flush's status.
    /// If an error prevents the flush from being scheduled, the error is indicated by the in-out `error` parameter.
    ///
    /// - Parameter error: On return, any error encountered while writing data, or `nil` if no error occurred.
    ///
    /// - Returns: A Boolean value indicating whether scheduling the metadata flush succeeded.
    @available(macOS 15.4, *)
    open func asynchronousMetadataFlush() throws

    /// Clears the given ranges within the buffer cache.
    ///
    /// This method clears the specified ranges in the resource’s buffer cache by writing zeroes into them.
    ///
    /// - Parameters:
    ///   - rangesToClear: The metadata ranges to clear.
    ///   - withDelayedWrites: A Boolean value that determines whether to perform the clear operation with delayed writes. The delay works in the same manner as ``delayedMetadataWriteFrom:startingAt:length:error:``. When using delayed writes, the client can flush the metadata with ``metadataFlushWithError:`` or ``asynchronousMetadataFlushWithError:``. The system also flushes stale data in the buffer cache periodically.
    ///   - error: On return, any error encountered while writing data, or `nil` if no error occurred. This value is `EINVAL` if `rangesToClear` is invalid.
    ///
    /// - Returns: A Boolean value indicating whether clearing the metadata succeeded.
    @available(macOS 15.4, *)
    open func metadataClear(_ rangesToClear: [FSMetadataRange], withDelayedWrites: Bool) throws

    /// Synchronously purges the given ranges from the buffer cache.
    ///
    /// This method removes the given ranges from the resource's buffer cache.
    /// This process drops any dirty data in the cache, preventing the data from reaching the device.
    ///
    /// - Parameters:
    ///   - rangesToPurge: The metadata ranges to purge.
    ///   - error: On return, any error encountered while writing data, or `nil` if no error occurred. This value is `EINVAL` if `rangesToPurge` is invalid.
    ///
    /// - Returns: A Boolean value indicating whether purging the metadata succeeded.
    @available(macOS 15.4, *)
    open func metadataPurge(_ rangesToPurge: [FSMetadataRange]) throws
}

@available(macOS 15.4, *)
@available(iOS, unavailable)
@available(watchOS, unavailable)
@available(tvOS, unavailable)
@available(visionOS, unavailable)
extension FSBlockDeviceResource {

    /// Asychronously reads data from the resource into a buffer.
    ///
    /// For the read to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector (`physicalBlockSize`) addressed operations of one or more sector-aligned offsets.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to receive the data.
    ///   - offset: The offset into the resource from which to start reading.
    ///   - length: A maximum number of bytes to read. The completion handler receives a parameter with the actual number of bytes read.
    ///
    /// - Returns: The actual number of bytes read.
    /// - Throws: An error describing any read error. This value is `EFAULT` if `buffer` is `NULL`, or `errno` if reading from the resource failed.
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) async throws -> Int

    /// Reads data from the resource into a buffer and executes a closure afterwards.
    ///
    /// For the read to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector (`physicalBlockSize`) addressed operations of one or more sector-aligned offsets.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to receive the data.
    ///   - offset: The offset into the resource from which to start reading.
    ///   - length: A maximum number of bytes to read. The completion handler receives a parameter with the actual number of bytes read.
    ///   - completionHandler: A closure that executes after the read operation completes. If successful, the first parameter contains the number of bytes actually read. In the case of an error, the second parameter contains a non-`nil` error. This value is `EFAULT` if `buffer` is `nil`, or `errno` if reading from the resource failed.
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int, completionHandler: @escaping (Int, (any Error)?) -> Void)

    /// Synchronously reads data from the resource into a buffer.
    ///
    /// This is a synchronous version of ``read(into:startingAt:length:)``.
    ///
    /// In some cases, this method performs a partial read. In this case, the return value is shorter than the requested length.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to receive the data.
    ///   - offset: The offset into the resource from which to start reading.
    ///   - length: A maximum number of bytes to read. The method's return value contains the actual number of bytes read.
    ///
    /// - Returns: The actual number of bytes read.
    /// - Throws: An error describing any read error. This value is `EFAULT` if `buffer` is `NULL`, or `errno` if reading from the resource failed.
    @available(macOS 15.4, *)
    public func read(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) throws -> Int

    /// Asynchronously writes data from from a buffer to the resource.
    ///
    /// For the write to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector (`physicalBlockSize`) addressed operations of one or more sector-aligned offsets.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to provide the data.
    ///   - offset: The offset into the resource from which to start writing.
    ///   - length: A maximum number of bytes to write. The completion handler receives a parameter with the actual number of bytes write.
    /// - Returns: The number of bytes written.
    /// - Throws: An error describing any write error. This value is `EFAULT` if `buffer` is `NULL`, or `errno` if writing to the resource failed.
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) async throws -> Int

    /// Writes data from from a buffer to the resource and executes a closure afterwards.
    ///
    /// For the read to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector (`physicalBlockSize`) addressed operations of one or more sector-aligned offsets.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to provide the data.
    ///   - offset: The offset into the resource from which to start writing.
    ///   - length: A maximum number of bytes to write. The completion handler receives a parameter with the actual number of bytes write.
    ///   - completionHandler: A closure that executes after the write operation completes. If successful, the first parameter contains the number of bytes actually written. In the case of an error, the second parameter contains a non-`nil` error. This value is `EFAULT` if `buffer` is `NULL`, or `errno` if writing to the resource failed.
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int, completionHandler: @escaping (Int, (any Error)?) -> Void)

    /// Synchronously writes data from from a buffer to the resource and executes a block afterwards.
    ///
    /// This method is a synchronous version of ``writeFrom:startingAt:length:completionHandler:``.
    ///
    /// In some cases, this method performs a partial write. In this case, the return value is shorter than the requested length, and the `error` is set to `nil`.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to provide the data.
    ///   - offset: The offset into the resource from which to start writing.
    ///   - length: A maximum number of bytes to write. The completion handler receives a parameter with the actual number of bytes write.
    ///
    /// - Returns: The actual number of bytes written.
    /// - Throws: Any error encountered while writing data.
    @available(macOS 15.4, *)
    public func write(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws -> Int

    /// Synchronously reads file system metadata from the resource into a buffer.
    ///
    /// This method provides access to the Kernel Buffer Cache, which is the primary system cache for file system metadata.
    /// Unlike equivalent kernel APIs, this method doesn't hold any kernel-level claim to the underlying buffers.
    ///
    /// For the read to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector-addressed operations of one or more sector-aligned offsets, where a sector equals `physicalBlockSize`.
    ///
    /// This method doesn't support partial reading of metadata.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to receive the data.
    ///   - offset: The offset into the resource from which to start reading.
    ///   - length: The number of bytes to read.
    /// - Throws: Any error encountered while reading data.
    @available(macOS 15.4, *)
    public func metadataRead(into buffer: UnsafeMutableRawBufferPointer, startingAt offset: off_t, length: Int) throws

    /// Synchronously writes file system metadata from a buffer to the resource.
    ///
    /// This method provides access to the Kernel Buffer Cache, which is the primary system cache for file system metadata.
    /// Unlike equivalent kernel APIs, this method doesn't hold any kernel-level claim to the underlying buffers.
    ///
    /// For the write to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector-addressed operations of one or more sector-aligned offsets, where a sector equals `physicalBlockSize`.
    ///
    /// This method doesn't support partial writing of metadata.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to provide the data.
    ///   - offset: The offset into the resource from which to start writing.
    ///   - length: The number of bytes to writing.
    ///
    /// - Throws: Any error encountered while reading data.
    @available(macOS 15.4, *)
    public func metadataWrite(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws

    /// Writes file system metadata from a buffer to a cache, prior to flushing it to the resource.
    ///
    /// This method provides access to the Kernel Buffer Cache, which is the primary system cache for file system metadata.
    /// Unlike equivalent kernel APIs, this method doesn't hold any kernel-level claim to the underlying buffers.
    ///
    /// This method is equivalent to ``metadataWriteFrom:startingAt:length:error:``, except that it writes data to the resource's buffer cache instead of writing to disk immediately.
    /// To ensure writing data to disk, the client must flush the metadata by calling ``metadataFlush()`` or ``asynchronousMetadataFlush()``.
    ///
    /// Delayed writes offer two significant advantages:
    /// - They perform more efficiently, since the file system can avoid waiting for the actual write, reducing I/O latency.
    /// - When writing to a specific range repeatedly, delayed writes allow the file system to flush data to the disk only when necessary, which reduces disk usage by eliminating unnecessary writes.
    ///
    /// For the write to succeed, requests must conform to any transfer requirements of the underlying resource.
    /// Disk drives typically require sector-addressed operations of one or more sector-aligned offsets, where a sector equals `physicalBlockSize`.
    ///
    /// This method doesn't support partial writing of metadata.
    ///
    /// - Parameters:
    ///   - buffer: A buffer to provide the data.
    ///   - offset: The offset into the resource from which to start writing.
    ///   - length: The number of bytes to writing.
    ///
    /// - Throws: Any error encountered while reading data.
    @available(macOS 15.4, *)
    public func delayedMetadataWrite(from buffer: UnsafeRawBufferPointer, startingAt offset: off_t, length: Int) throws
}

/// A resource that represents an abstract URL.
///
/// An `FSGenericURLResource` is a completely abstract resource.
/// The only reference to its contents is a single URL, the contents of which are arbitrary.
/// This URL might represent a PCI locator string like `/pci@f0000000/usb@5`, or some sort of network address for a remote file system.
/// FSKit leaves interpretation of the URL and its contents entirely up to your implementation.
///
/// Use the `Info.plist` key `FSSupportedSchemes` to provide an array of case-insensitive URL schemes that your implementation supports.
/// The following example shows how a hypothetical `FSGenericURLResource` implementation declares support for the `rsh` and `ssh` URL schemes:
/// ```
/// <key>FSSupportedSchemes</key>
/// <array>
///     <string>rsh</string>
///     <string>ssh</string>
/// </array>
/// ```
@available(macOS 26.0, *)
open class FSGenericURLResource : FSResource {

    /// The URL represented by the resource.
    open var url: URL { get }

    /// Creates a generic URL resource with the given URL.
    /// - Parameter url: A URL that provides the content of the file system. The format of this URL is completely arbitrary. It's up to your extension to access the contents represented by the URL and make them available as an ``FSVolume`` that FSKit can load.
    public init(url: URL)
}

/// A resource that represents a path in the system file space.
///
/// The URL passed to `FSPathURLResource` may be a security-scoped URL.
/// If the URL is a security-scoped URL, FSKit transports it intact from a client application to your extension.
@available(macOS 26.0, *)
open class FSPathURLResource : FSResource {

    /// The URL represented by the resource.
    open var url: URL { get }

    /// Creates a path URL resource.
    /// - Parameters:
    ///   - URL: A URL in the system file space that represents the contents of a file system. This parameter uses the `file:` scheme.
    ///   - writable: A Boolean value that indicates whether the file system supports writing to the contents of the URL.
    public init(url URL: URL, writable: Bool)

    /// A Boolean value that indicates whether the file system supports writing to the contents of the path URL.
    open var isWritable: Bool { get }
}

/// Maintenance operations for a file system's resources.
///
/// This protocol includes operations to check and format a resource for an ``FSUnaryFileSystem``.
/// Conform to this protocol if you implement a ``FSUnaryFileSystem`` that uses an ``FSBlockDeviceResource-c.class``.
@available(macOS 15.4, *)
public protocol FSManageableResourceMaintenanceOperations : NSObjectProtocol {

    /// Starts checking the file system with the given options.
    ///
    /// - Parameters:
    ///   - task: A task object you use to communicate back to the client.
    ///   - options: Options for performing the check.
    ///   - error: In Objective-C, a pointer to an <doc://com.apple.documentation/documentation/Foundation/NSError>. Populate this with any error that occurs when starting the check. In Swift, throw an <doc://com.apple.documentation/documentation/Swift/Error> instead.
    /// - Returns: An <doc://com.apple.documentation/documentation/Foundation/Progress> object that you use to update progress as the check operation progresses. Return `nil` if starting the file system check encountered an error.
    @available(macOS 15.4, *)
    func startCheck(task: FSTask, options: FSTaskOptions) throws -> Progress

    /// Starts formatting the file system with the given options.
    ///
    /// - Parameters:
    ///   - task: A task object you use to communicate back to the client.
    ///   - options: Options for performing the format.
    ///   - error: In Objective-C, a pointer to an <doc://com.apple.documentation/documentation/Foundation/NSError>. Populate this with any error that occurs when starting the format. In Swift, throw an <doc://com.apple.documentation/documentation/Swift/Error> instead.
    /// - Returns: An <doc://com.apple.documentation/documentation/Foundation/Progress> object that you use to update progress as the format operation progresses. Return `nil` if starting to format the file system encountered an error.
    @available(macOS 15.4, *)
    func startFormat(task: FSTask, options: FSTaskOptions) throws -> Progress
}

/// An object that represents the results of a specific probe.
///
/// For any ``result`` value other than ``FSMatchResult/notRecognized``, ensure the ``name`` and ``containerID`` values are non-`nil`.
/// When a container or volume format doesn't use a name, return an empty string.
/// Also use an empty string in the case in which the format supports a name, but the value isn't set yet.
///
/// Some container or volume formats may lack a durable UUID on which to base a container identifier.
/// This situation is only valid for unary file systems.
/// In such a case, return a random UUID.
///
/// With a block device resource, a probe operation may successfully get a result but encounter an error reading the name or UUID.
/// If this happens, use whatever information is available, and provide an empty string or random UUID for the name or container ID, respectively.
@available(macOS 15.4, *)
open class FSProbeResult : NSObject, NSSecureCoding {

    /// The match result, representing the recognition and usability of a probed resource.
    open var result: FSMatchResult { get }

    /// The resource name, as found during the probe operation.
    ///
    /// This value is non-`nil` unless the ``FSProbeResult/result`` is ``FSMatchResult/notRecognized`.
    /// For formats that lack a name, this value may be an empty string.
    /// This value can also be an empty string if the format supports a name, but the value isn't set yet.
    open var name: String? { get }

    /// The container identifier, as found during the probe operation.
    ///
    /// This value is non-`nil` unless the ``FSProbeResult/result`` is ``FSMatchResult/notRecognized``.
    /// For formats that lack a durable UUID on which to base a container identifier --- which is only legal for a ``FSUnaryFileSystem`` --- this value may be a random UUID.
    open var containerID: FSContainerIdentifier? { get }

    /// A probe result for an unrecognized file system.
    ///
    /// An unrecognized probe result contains `nil` for its ``FSProbeResult/name`` and ``FSProbeResult/containerID`` properties.
    open class var notRecognized: FSProbeResult { get }

    /// Creates a probe result for a recognized file system.
    ///
    /// - Parameters:
    ///   - name: The resource name, as found during the probe operation. If the file system doesn't support names, or is awaiting naming, use an empty string.
    ///   - containerID: The container identifier, as found during the probe operation. If the file system doesn't support durable identifiers, use a random UUID.
    open class func recognized(name: String, containerID: FSContainerIdentifier) -> Self

    /// A probe result for a recognized file system that is usable, but with limited capabilities.
    ///
    /// This kind of probe result lacks the ``FSProbeResult/name``, ``FSProbeResult/containerID``, or both.
    /// Don't return this result from probing a resource that isn't limited.
    open class var usableButLimited: FSProbeResult { get }

    /// Creates a probe result for a recognized file system that is usable, but with limited capabilities.
    ///
    /// - Parameters:
    ///   - name: The resource name, as found during the probe operation. If the file system doesn't support names, or is awaiting naming, use an empty string.
    ///   - containerID: The container identifier, as found during the probe operation. If the file system doesn't support durable identifiers, use a random UUID.
    open class func usableButLimited(name: String, containerID: FSContainerIdentifier) -> Self

    /// Creates a probe result for a recognized and usable file system.
    ///
    /// - Parameters:
    ///   - name: The resource name, as found during the probe operation. If the file system doesn't support names, or is awaiting naming, use an empty string.
    ///   - containerID: The container identifier, as found during the probe operation. If the file system doesn't support durable identifiers, use a random UUID.
    open class func usable(name: String, containerID: FSContainerIdentifier) -> Self
}

/// A class that enables a file system module to pass log messages and completion notifications to clients.
///
/// FSKit creates an instance of this class for each long-running operations.
@available(macOS 15.4, *)
open class FSTask : NSObject, @unchecked Sendable {

    /// Logs the given string to the initiating client.
    ///
    /// - Parameter str: The string to log.
    @available(macOS 15.4, *)
    open func logMessage(_ str: String)

    /// Informs the client that the task completed.
    ///
    /// - Parameter error: `nil` if the task completed successfully; otherwise, an error that caused the task to fail.
    @available(macOS 15.4, *)
    open func didComplete(error: (any Error)?)

    /// A handler called by FSKit upon canceling the task.
    ///
    /// FSKit calls the cancellation handler within an independent execution context.
    ///
    /// If the handler can't complete its work successfully, it can return an error from the block or closure.
    /// FSKit logs any returned error and then terminates all activity in the container.
    ///
    /// The task object clears its `cancellationHandler` property after the task's cancellation or completion.
    /// This helps accelerate the cleanup of retained state.
    ///
    /// The exact structuring of the completion handler depends on the structuring of the code implementing the task.
    /// As a concrete example, consider a check operation with the following class:
    ///
    /// @TabNavigator {
    ///   @Tab("Objective-C") {
    ///     ```obj-c
    ///     @‎‌interface YourFileSystem : NSObject
    ///     @property (retain) dispatch_group_t work_group;
    ///     @property (nonatomic,getter=interrupted) BOOL interrupted;
    ///     @end
    ///     ```
    ///   }
    ///   @Tab("Swift") {
    ///     ```swift
    ///     class YourFileSystem {
    ///         let work_group: dispatch_group_t = DispatchGroup()
    ///         var interrupted: Bool = false
    ///         private lazy var leaveWorkGroupOnce = leaveWorkGroup()
    ///     }
    ///     ```
    ///   }
    /// }
    ///
    /// and a `startCheckWithTask` method with a helper method `performCheck` like the following:
    ///
    /// @TabNavigator {
    ///   @Tab("Objective-C") {
    ///     ```obj-c
    ///     - (void)performCheck:(nonnull FSTask *)task
    ///                 progress:(nonnull NSProgress *)progress
    ///                  context:(YourFileSystemCancelationContext *)context {
    ///                  // See discussion for notes on implementing this method.
    ///       }
    ///
    ///     - (NSProgress * _Nullable)startCheckWithTask:(nonnull FSTask *)task
    ///                                          options:(nonnull FSTaskOptions *)options
    ///                                            error:(NSError *__autoreleasing  _Nullable * _Nullable)error {
    ///         dispatch_queue_t someQueue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0);
    ///         NSProgress *progress = [[NSProgress alloc] init];
    ///         YourFileSystemCancelationContext *checker= [[YourFileSystemCancelationContext alloc] init];
    ///         dispatch_group_enter(checker.work_group);
    ///         [task setCancellationHandler:^NSError * _Nullable{
    ///             checker.interrupted = YES;
    ///             dispatch_group_wait(checker.work_group, DISPATCH_TIME_FOREVER);
    ///             return nil;
    ///         }];
    ///         dispatch_async(someQueue, ^{[self performCheck:task progress:progress context:checker];});
    ///         return progress;
    ///     }
    ///     ```
    ///   }
    ///   @Tab("Swift") {
    ///     ```swift
    ///     func performCheck(task: FSTask, progress: Progress,
    ///                       context: YourFileSystemCancelationContext) {
    ///        // See discussion for notes on implementing this method.
    ///     }
    ///
    ///     func startCheck(task: FSTask, options: FSTaskOptions) throws -> Progress {
    ///         let someQueue = DispatchQueue.global()
    ///         let progress = Progress()
    ///         let checkContext = YourFileSystemCancelationContext()
    ///         checkContext.work_group.enter()
    ///         task.cancellationHandler = {
    ///             checkContext.interrupted = true
    ///             checkContext.work_group.wait()
    ///             return nil
    ///         }
    ///         someQueue.async {
    ///             self.performCheck(task: task, progress: progress, context:checkContext)
    ///         }
    ///         return progress
    ///     }
    ///     ```
    ///   }
    /// }
    ///
    /// When canceled, the handler block in this example sets the checker's `interrupted` property, and then calls the <doc://com.apple.documentation/documentation/dispatch/dispatchgroup> method <doc://com.apple.documentation/documentation/dispatch/dispatchgroup/wait()> (Swift) or the function <doc://com.apple.documentation/documentation/dispatch/dispatch_group_wait> (Objective-C) on the checker's work group.
    /// Because neither of these operations can fail, the handler returns `nil` to indicate it didn't encounter an error.
    ///
    /// For simplicity, this example doesn't account for errors, whereas production code must do so.
    /// Furthermore, when fully implemented, the `performCheck` method should perform a check operation.
    /// Specifically, it should periodically update the progress object and check its `interrupted` variable.
    /// The check can either complete successfully, complete with an error, or enter the interrupted state.
    /// It should then call ``FSTask/didComplete(error:)`` with the appropriate error value or `nil`.
    /// Finally it should call `context.work_group.leave()` (Swift) or `dispatch_group_leave(context.work_group)` (Objective-C) to remove itself from its dispatch group.
    @available(macOS 26.0, *)
    open var cancellationHandler: (@Sendable () -> (any Error)?)?
}

/// A class that passes command options to a task, optionally providing security-scoped URLs.
@available(macOS 15.4, *)
open class FSTaskOptions : NSObject {

    /// An array of strings that represent command-line options for the task.
    ///
    /// This property is equivalent to the `argv` array of C strings passed to a command-line tool.
    open var taskOptions: [String] { get }

    /// Retrieves a URL for a given option.
    ///
    /// Some command-line options refer to paths that indicate a location in which the module needs access to a file outside of its container.
    /// FSKit passes these paths as a URL tagged by the option name.
    ///
    /// For example, `"-B" "./someFile"` returns the URL for `./someFile` when passed an option `"B"`.
    /// To indicate that your module treats a given option as a path, include it in the `pathOptions` dictionary within a command options dictionary (`FSActivatOptionSyntax`, `FSCheckOptionSyntax`, or `FSFormatOptionSyntax`).
    /// This dictionary uses the command option name as a key, and each entry has a value indicating what kind of entry to create.
    ///
    /// - Parameter option: The option for which to retrieve the URL. This value doesn't include leading dashes.
    open func url(forOption option: String) -> URL?
}

/// An abstract base class for implementing a minimal file system.
///
/// `FSUnaryFileSystem` is a simplified file system, which works with one ``FSResource`` and presents it as one ``FSVolume``.
///
/// The one volume and its container have a shared state and lifetime, a more constrained life cycle than the ``FSFileSystem`` design flow.
///
/// Implement your app extension by providing a subclass of `FSUnaryFileSystem` as a delegate object.
/// Your delegate also needs to implement the ``FSUnaryFileSystemOperations`` protocol so that it can load resources.
@available(macOS 15.4, *)
open class FSUnaryFileSystem : NSObject, FSFileSystemBase {
}

/// Operations performed by a unary file system.
///
/// Make sure your subclass of ``FSUnaryFileSystem`` conforms to this protocol.
@available(macOS 15.4, *)
public protocol FSUnaryFileSystemOperations : NSObjectProtocol {

    /// Requests that the file system probe the specified resource.
    ///
    /// Implement this method to indicate whether the resource is recognizable and usable.
    /// - Parameters:
    ///   - resource: The ``FSResource`` to probe.
    ///   - reply: A block or closure that your implementation invokes when it finishes the probe or encounters an error. Pass an instance of ``FSProbeResult`` with probe results as the first parameter if your probe operation succeeds. If probing fails, pass an error as the second parameter.
    @available(macOS 15.4, *)
    func probeResource(resource: FSResource, replyHandler reply: @escaping @Sendable (FSProbeResult?, (any Error)?) -> Void)

    /// Requests that the file system probe the specified resource.
    ///
    /// Implement this method to indicate whether the resource is recognizable and usable.
    /// - Parameters:
    ///   - resource: The ``FSResource`` to probe.
    ///   - reply: A block or closure that your implementation invokes when it finishes the probe or encounters an error. Pass an instance of ``FSProbeResult`` with probe results as the first parameter if your probe operation succeeds. If probing fails, pass an error as the second parameter.
    @available(macOS 15.4, *)
    func probeResource(resource: FSResource) async throws -> FSProbeResult

    /// Requests that the file system load a resource and present it as a volume.
    ///
    /// Implement this method by inspecting the provided resource and verifying it uses a supported format.
    /// If the resource does use a supported format, create a subclass of `FSVolume`, clear the container error state, and invoke the `reply` callback, passing your volume as a parameter.
    /// If loading can't proceed, invoke `reply` and send an appropriate error as the second parameter.
    ///
    /// - Parameters:
    ///   - resource: An ``FSResource`` to load.
    ///   - options: An ``FSTaskOptions`` object specifying options to apply when loading the resource. An ``FSUnaryFileSystem`` supports two options: `-f` for "force" and `--rdonly` for read-only. The file system must remember if the read-only option is present.
    ///   - reply: A block or closure that your implementation invokes when it finishes setting up or encounters an error. Pass a subclass of `FSVolume` as the first parameter if loading succeeds. If loading fails, pass an error as the second parameter.
    @available(macOS 15.4, *)
    func loadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void)

    /// Requests that the file system load a resource and present it as a volume.
    ///
    /// Implement this method by inspecting the provided resource and verifying it uses a supported format.
    /// If the resource does use a supported format, create a subclass of `FSVolume`, clear the container error state, and invoke the `reply` callback, passing your volume as a parameter.
    /// If loading can't proceed, invoke `reply` and send an appropriate error as the second parameter.
    ///
    /// - Parameters:
    ///   - resource: An ``FSResource`` to load.
    ///   - options: An ``FSTaskOptions`` object specifying options to apply when loading the resource. An ``FSUnaryFileSystem`` supports two options: `-f` for "force" and `--rdonly` for read-only. The file system must remember if the read-only option is present.
    ///   - reply: A block or closure that your implementation invokes when it finishes setting up or encounters an error. Pass a subclass of `FSVolume` as the first parameter if loading succeeds. If loading fails, pass an error as the second parameter.
    @available(macOS 15.4, *)
    func loadResource(resource: FSResource, options: FSTaskOptions) async throws -> FSVolume

    /// Requests that the file system unload the specified resource.
    ///
    /// - Parameters:
    ///   - resource: An ``FSResource`` to unload.
    ///   - options: An ``FSTaskOptions`` object specifying options to apply when unloading the resource.
    ///   - reply: A block or closure that your implementation invokes when it finishes unloading or encounters an error. If unloading fails, pass an error as the parameter to describe the problem. Otherwise, pass `nil`.
    @available(macOS 15.4, *)
    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

    /// Requests that the file system unload the specified resource.
    ///
    /// - Parameters:
    ///   - resource: An ``FSResource`` to unload.
    ///   - options: An ``FSTaskOptions`` object specifying options to apply when unloading the resource.
    ///   - reply: A block or closure that your implementation invokes when it finishes unloading or encounters an error. If unloading fails, pass an error as the parameter to describe the problem. Otherwise, pass `nil`.
    @available(macOS 15.4, *)
    func unloadResource(resource: FSResource, options: FSTaskOptions) async throws

    /// Notifies you that the system finished loading your file system extension.
    ///
    /// The system performs this callback after the main run loop starts and before receiving the first message from the FSKit daemon.
    ///
    /// Implement this method if you want to perform any setup prior to receiving FSKit callbacks.
    @available(macOS 15.4, *)
    optional func didFinishLoading()
}

/// A value that indicates a location in a directory from which to enumerate.
///
/// Your implementation of ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:context:replyHandler:)`` defines the semantics of this value; it's opaque to FSKit.
@available(macOS 15.4, *)
public struct FSDirectoryCookie : Hashable, Equatable, RawRepresentable, @unchecked Sendable {

    public init(_ rawValue: UInt64)

    public init(rawValue: UInt64)
}

extension FSDirectoryCookie {

    /// The constant initial value for the directory-enumeration cookie.
    @available(macOS 15.4, *)
    public static let initial: FSDirectoryCookie
}

/// A tool to detect whether the directory contents changed since the last call to enumerate a directory.
///
/// Your implementation of ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:replyHandler:)`` defines the semantics of this value; it's opaque to FSKit.
@available(macOS 15.4, *)
public struct FSDirectoryVerifier : Hashable, Equatable, RawRepresentable, @unchecked Sendable {

    public init(_ rawValue: UInt64)

    public init(rawValue: UInt64)
}

extension FSDirectoryVerifier {

    /// The constant initial value for the directory-enumeration verifier.
    @available(macOS 15.4, *)
    public static let initial: FSDirectoryVerifier
}

/// Options that affect the behavior of deactivate methods.
@available(macOS 15.4, *)
public struct FSDeactivateOptions : OptionSet, @unchecked Sendable {

    public init(rawValue: Int)

    /// An option to force deactivation.
    public static var force: FSDeactivateOptions { get }
}

/// Behavior flags for use with synchronization calls.
///
/// These values are based on flags defined in `mount.h`. Since there are system-defined flags that are valid in the kernel but not in FSKit, this type defines its members as options rather than use an enumeration.
@available(macOS 15.4, *)
public enum FSSyncFlags : Int, @unchecked Sendable {

    /// A flag for synchronized I/O with file-integrity completion.
    case wait = 1

    /// A flag for synchronized I/O that starts I/O but doesn't wait for it.
    case noWait = 2

    /// A flag for synchronized I/O with data-integrity completion.
    case dWait = 4
}

/// An object used to provide items during a directory enumeration.
///
/// You use this type in your implementation of ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:context:replyHandler:)``.
///
/// Packing allows your implementation to provide information FSKit needs, including each item's name, type, and identifier (such as an inode number).
/// Some directory enumerations require other attributes, as indicated by the ``FSItemGetAttributesRequest`` sent to the enumerate method.
@available(macOS 15.4, *)
open class FSDirectoryEntryPacker : NSObject {

    /// Provides a directory entry during enumeration.
    ///
    /// You call this method in your implementation of ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:context:replyHandler:)``, for each directory entry you want to provide to the enumeration.
    ///
    /// - Parameters:
    ///   - name: The item's name.
    ///   - itemType: The type of the item.
    ///   - itemID: The item's identifier. Typically this is an inode number, or one of the constants defined by ``FSItem/Identifier`` like ``FSItem/Identifier/rootDirectory``.
    ///   - nextCookie: A value to indicate the next entry in the directory to enumerate. FSKit passes this value as the `cookie` parameter on the next call to ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:context:replyHandler:)``. Use whatever value is appropriate for your implementation; the value is opaque to FSKit.
    ///   - attributes: The item's attributes. Pass `nil` if the enumeration call didn't request attributes.
    /// - Returns: `true` (Swift) or `YES` (Objective-C) if packing was successful and enumeration can continue with the next directory entry. If the value is `false` (Swift) or `NO` (Objective-C), stop enumerating. This result can happen when the entry is too big for the remaining space in the buffer.
    @available(macOS 15.4, *)
    open func packEntry(name: FSFileName, itemType: FSItem.ItemType, itemID: FSItem.Identifier, nextCookie: FSDirectoryCookie, attributes: FSItem.Attributes?) -> Bool
}

/// A directory structure for files and folders.
///
/// A file system, depending on its type, provides one or more volumes to clients.
/// The ``FSUnaryFileSystem`` by definition provides only one volume, while an ``FSFileSystem`` supports multiple volumes.
///
/// You implement a volume for your file system type by subclassing this class, and also conforming to the ``FSVolume/Handler`` and ``FSVolume/PathConfOperations`` protocols.
/// This protocol defines the minimum set of operations supported by a volume, such as mounting, activating, creating and removing items, and more.
///
/// Your volume can provide additional functionality by conforming to other volume handler protocols.
/// These protocols add support for operations like open and close, read and write, extended attribute (Xattr) manipulation, and more.
@available(macOS 15.4, *)
open class FSVolume : NSObject {

    /// An identifier that uniquely identifies the volume.
    open var volumeID: FSVolume.Identifier { get }

    /// The name of the volume.
    @NSCopying open var name: FSFileName

    /// Creates a volume with the given identifier and name.
    /// - Parameters:
    ///   - volumeID: An ``FSVolumeIdentifier`` to uniquely identify the volume. For a network file system that supports multiple authenticated users, disambiguate the users by using qualifying data in the identifier.
    ///   - volumeName: A name for the volume.
    public init(volumeID: FSVolume.Identifier, volumeName: FSFileName)
}

@available(macOS 15.4, *)
extension FSVolume : Identifiable {

    /// The stable identity of the entity associated with this instance.
    @available(macOS 15.4, *)
    public var id: String { get }

    /// A type representing the stable identity of the entity associated with
    /// an instance.
    @available(macOS 15.4, *)
    public typealias ID = String
}

extension FSVolume {

    /// Properties implemented by volumes that support providing the values of system limits or options.
    ///
    /// This protocol gathers properties related to the `pathconf` and `fpathconf` system calls.
    ///
    /// For a file, the value of a property applies to just that file; for a directory, the value applies to all items in the directory.
    ///
    /// Properties that represent limits and have a numeric type use `-1` to represent no limit.
    @available(macOS 15.4, *)
    public protocol PathConfOperations : NSObjectProtocol {

        /// A property that represents the maximum number of hard links to the object.
        var maximumLinkCount: Int { get }

        /// A property that represents the maximum length of a component of a filename.
        var maximumNameLength: Int { get }

        /// A Boolean property that indicates whether the volume restricts ownership changes based on authorization.
        ///
        /// If this value is true, the volume rejects a `chown(2)` from anyone other than the superuser.
        var restrictsOwnershipChanges: Bool { get }

        /// A property that indicates whether the volume truncates files longer than its maximum supported length.
        ///
        /// If this value is `true`, the volume truncates the filename to ``maximumNameLength`` if the filename is longer than that.
        /// If this value is false, the file system responds with the error code `ENAMETOOLONG` if the filename is longer than ``maximumNameLength``.
        var truncatesLongNames: Bool { get }

        /// The maximum extended attribute size in bytes.
        ///
        /// Implement at least one of `maximumXattrSize` or ``maximumXattrSizeInBits``.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumXattrSizeInBits` implementation.
        optional var maximumXattrSize: Int { get }

        /// The maximum extended attribute size in bits.
        ///
        /// Implement at least one of ``maximumXattrSize`` or `maximumXattrSizeInBits`.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumXattrSizeInBits` implementation.
        optional var maximumXattrSizeInBits: Int { get }

        /// The maximum size of a regular file allowed in the volume.
        ///
        /// Implement at least one of `maximumFileSize` or ``maximumFileSizeInBits``.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumFileSizeInBits` implementation.
        optional var maximumFileSize: UInt64 { get }

        /// The minimum number of bits needed to represent, as a signed integer value, the maximum size of a regular file allowed in the volume.
        ///
        /// The maximum file size is `2^(maximumFileSizeInBits - 1)`.
        ///
        /// | Maximum file size (bytes)  | Maximum (in hex)     | Unsigned bits | Signed bits |
        /// | -------------------------: | -------------------: | ------------: | ----------: |
        /// |                     65,535 |             `0xFFFF` |            16 |          17 |
        /// |              2,147,483,647 |         `0x7FFFFFFF` |            31 |          32 |
        /// |              4,294,967,295 |         `0xFFFFFFFF` |            32 |          33 |
        /// | 18,446,744,073,709,551,615 | `0xFFFFFFFFFFFFFFFF` |            64 |          65 |
        ///
        /// Implement at least one of ``maximumFileSize`` or `maximumFileSizeInBits`.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumFileSizeInBits` implementation.
        optional var maximumFileSizeInBits: Int { get }
    }

    /// Methods common to `FSVolumeHandler` and `FSVolumeOperations`
    @available(macOS 15.4, *)
    public protocol CommonOperations {

        /// A property that provides the supported capabilities of the volume.
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }

        /// A property that provides up-to-date statistics of the volume.
        var volumeStatistics: FSStatFSResult { get }

        /// A property that allows the file system to use open-unlink emulation.
        ///
        /// _Open-unlink_ functionality refers to a file system's ability to support an open file being fully unlinked from the file system namespace.
        /// If a file system doesn't support this functionality, FSKit can emulate it instead; this is called "open-unlink emulation".
        ///
        /// Implement this property to return `true` (Swift) or `YES` (Objective-C) to allow FSKit to perform open-unlink emulation.
        /// If you don't implement this property at all, FSKit doesn't perform open-unlink emulation for this volume.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        @available(macOS 26.0, *)
        optional var enableOpenUnlinkEmulation: Bool { get }

        /// A property that allows the file system to request for specific mount options from FSKit.
        ///
        /// FSKit reads this value after the volume replies to the ``mount(options:)`` call.
        /// Changing the returned value during the runtime of the volume has no effect.
        @available(macOS 26.4, *)
        optional var requestedMountOptions: FSVolume.MountOptions { get }

        /// Mounts this volume, using the specified options.
        ///
        /// FSKit calls this method as a signal that some process is trying to mount this volume.
        /// Your file system receives a call to ``activate(options:)`` prior to receiving any mount calls.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the mount. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If mounting fails, pass an error as the one parameter to the reply handler. If mounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Mounts this volume, using the specified options.
        ///
        /// FSKit calls this method as a signal that some process is trying to mount this volume.
        /// Your file system receives a call to ``activate(options:)`` prior to receiving any mount calls.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the mount. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If mounting fails, pass an error as the one parameter to the reply handler. If mounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func mount(options: FSTaskOptions) async throws

        /// Unmounts this volume.
        ///
        /// Clear and flush all cached state in your implementation of this method.
        ///
        /// - Parameters:
        ///   - reply: A block or closure to indicate success or failure. If unmounting fails, pass an error as the one parameter to the reply handler. If unmounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)

        /// Unmounts this volume.
        ///
        /// Clear and flush all cached state in your implementation of this method.
        ///
        /// - Parameters:
        ///   - reply: A block or closure to indicate success or failure. If unmounting fails, pass an error as the one parameter to the reply handler. If unmounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func unmount() async

        /// Synchronizes the volume with its underlying resource.
        ///
        /// After calling this method, FSKit assumes that the volume has sent all pending I/O or metadata to its resource.
        ///
        /// - Parameters:
        ///   - flags: Timing flags, as defined in `mount.h.` These flags let the file system know whether to run the operation in a blocking or nonblocking fashion.
        ///   - reply: A block or closure to indicate success or failure. If synchronization fails, pass an error as the one parameter to the reply handler. If synchronization succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Synchronizes the volume with its underlying resource.
        ///
        /// After calling this method, FSKit assumes that the volume has sent all pending I/O or metadata to its resource.
        ///
        /// - Parameters:
        ///   - flags: Timing flags, as defined in `mount.h.` These flags let the file system know whether to run the operation in a blocking or nonblocking fashion.
        ///   - reply: A block or closure to indicate success or failure. If synchronization fails, pass an error as the one parameter to the reply handler. If synchronization succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func synchronize(flags: FSSyncFlags) async throws

        /// Reclaims an item, releasing any resources allocated for the item.
        ///
        /// FSKit guarantees that for every ``FSItem`` returned by the volume, a corresponding reclaim operation occurs after the upper layers no longer reference that item. To use this behavior, call ``FSItem/tryReclaim(_:)`` in your implementation of this method.
        ///
        /// > Note: Block device file systems may assess whether an underlying resource terminates before processing reclaim operations. On unary file systems, for example, the associated volumes unmount when such resources disconnect from the system. The unmount triggers a reclaiming of all items. Some implementations benefit greatly from short-circuiting in such cases. With a terminated resource, all I/O results in an error, making short-circuiting the most efficient response.
        ///
        /// - Parameters:
        ///   - item: The item to reclaim.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Reclaims an item, releasing any resources allocated for the item.
        ///
        /// FSKit guarantees that for every ``FSItem`` returned by the volume, a corresponding reclaim operation occurs after the upper layers no longer reference that item. To use this behavior, call ``FSItem/tryReclaim(_:)`` in your implementation of this method.
        ///
        /// > Note: Block device file systems may assess whether an underlying resource terminates before processing reclaim operations. On unary file systems, for example, the associated volumes unmount when such resources disconnect from the system. The unmount triggers a reclaiming of all items. Some implementations benefit greatly from short-circuiting in such cases. With a terminated resource, all I/O results in an error, making short-circuiting the most efficient response.
        ///
        /// - Parameters:
        ///   - item: The item to reclaim.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func reclaimItem(_ item: FSItem) async throws
    }

    /// Methods that all volumes implement to provide required capabilities.
    ///
    /// Conform to this protocol in your subclass of ``FSVolume``.
    /// To provide additional capabilities, conform to the other `FSVolume` operations protocols, such as ``FSVolumeOpenCloseOperations`` and ``FSVolumeReadWriteOperations``.
    ///
    /// > Note: This protocol extends ``FSVolumePathConfOperations``, so your volume implementation must also conform to that protocol.
    ///
    /// > Deprecated: Use ``FSVolume/Handler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol Operations : FSVolume.CommonOperations, FSVolume.PathConfOperations {

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass the root ``FSItem`` and a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. In Swift, `reply` takes only the ``FSItem`` as the parameter; you signal any error with a `throw`. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func activate(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSItem?, (any Error)?) -> Void)

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass the root ``FSItem`` and a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. In Swift, `reply` takes only the ``FSItem`` as the parameter; you signal any error with a `throw`. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func activate(options: FSTaskOptions) async throws -> FSItem

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount()`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivate(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount()`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivate(options: FSDeactivateOptions = []) async throws

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName`` (as saved within the file system), along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; any ``FSItem`` or ``FSFileName`` are ignored in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` and ``FSFileName`` as a tuple or throw an error.
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName`` (as saved within the file system), along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; any ``FSItem`` or ``FSFileName`` are ignored in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` and ``FSFileName`` as a tuple or throw an error.
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem) async throws -> (FSItem, FSFileName)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest) async throws -> (FSItem, FSFileName)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName) async throws -> (FSItem, FSFileName)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an ``FSFileName`` of the newly-created link and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an ``FSFileName`` of the newly-created link and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem) async throws -> FSFileName

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:)``. After doing so, ensure the operation finishes without errors.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass the ``FSFileName`` as it exists within `destinationDirectory` and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:)``. After doing so, ensure the operation finishes without errors.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass the ``FSFileName`` as it exists within `destinationDirectory` and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?) async throws -> FSFileName

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem) async throws

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an ``FSItemAttributes`` with the requested attributes populated and a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an ``FSItemAttributes`` with the requested attributes populated and a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem) async throws -> FSItem.Attributes

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an ``FSItemAttributes`` with the item's updated attributes and a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an ``FSItemAttributes`` with the item's updated attributes and a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem) async throws -> FSItem.Attributes

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass the current verifier and a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores any verifier in this case. For an `async` Swift implementation, there's no reply handler; simply return the current verifier or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, replyHandler reply: @escaping @Sendable (FSDirectoryVerifier, (any Error)?) -> Void)

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass the current verifier and a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores any verifier in this case. For an `async` Swift implementation, there's no reply handler; simply return the current verifier or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker) async throws -> FSDirectoryVerifier

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the link's contents as an ``FSFileName`` and a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func readSymbolicLink(_ item: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the link's contents as an ``FSFileName`` and a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func readSymbolicLink(_ item: FSItem) async throws -> FSFileName
    }

    /// Methods that all volumes implement to provide required capabilities.
    ///
    /// Conform to this protocol in your subclass of ``FSVolume``.
    /// To provide additional capabilities, conform to the other `FSVolume` handler protocols, such as ``FSVolumeOpenCloseHandler`` and ``FSVolumeReadWriteHandler``.
    ///
    /// > Note: This protocol extends ``FSVolumePathConfOperations``, so your volume implementation must also conform to that protocol.
    ///
    /// > Important: This protocol replaces the ``FSVolumeOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol Handler : FSVolume.CommonOperations, FSVolume.PathConfOperations {

        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }

        /// A property that provides up-to-date statistics of the volume.
        var volumeStatistics: FSStatFSResult { get }

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass an instance of ``FSActivateResult`` containing the root ``FSItem``, along with a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSActivateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void)

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass an instance of ``FSActivateResult`` containing the root ``FSItem``, along with a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSActivateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func activateVolume(options: FSTaskOptions) async throws -> FSActivateResult

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount(replyHandler:)`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateVolume(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount(replyHandler:)`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateVolume(options: FSDeactivateOptions = []) async throws

        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func mount(options: FSTaskOptions) async throws

        func unmount(replyHandler reply: @escaping @Sendable () -> Void)

        func unmount() async

        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func synchronize(flags: FSSyncFlags) async throws

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemResult`` containing the found ``FSItem``, its ``FSFileName`` (as saved within the file system), and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemResult`` containing the found ``FSItem``, its ``FSFileName`` (as saved within the file system), and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSLookupItemResult

        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func reclaimItem(_ item: FSItem) async throws

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateItemResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateItemResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext) async throws -> FSCreateItemResult

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateSymlinkResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateSymlinkResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext) async throws -> FSCreateSymlinkResult

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateLinkResult`` containing the ``FSFileName`` of the newly-created link, the ``FSItemAttributes`` of the linked item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateLinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateLinkResult`` containing the ``FSFileName`` of the newly-created link, the ``FSItemAttributes`` of the linked item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateLinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSCreateLinkResult

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:replyHandler:)``. After doing so, ensure the operation finishes without errors.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSRenameItemResult`` containing the ``FSFileName`` as it exists within `destinationDirectory`, the ``FSItemAttributes`` of the renamed item, the updated ``FSItemAttributes`` of the source directory, the updated ``FSItemAttributes`` of the destination directory, the ``FSItemAttributes`` of the overwritten item (if any), and the volume's updated free space, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRenameItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:replyHandler:)``. After doing so, ensure the operation finishes without errors.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSRenameItemResult`` containing the ``FSFileName`` as it exists within `destinationDirectory`, the ``FSItemAttributes`` of the renamed item, the updated ``FSItemAttributes`` of the source directory, the updated ``FSItemAttributes`` of the destination directory, the ``FSItemAttributes`` of the overwritten item (if any), and the volume's updated free space, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRenameItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext) async throws -> FSRenameItemResult

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If removal succeeds, pass an instance of ``FSRemoveItemResult`` containing the ``FSItemAttributes`` of the removed item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If removal fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRemoveItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If removal succeeds, pass an instance of ``FSRemoveItemResult`` containing the ``FSItemAttributes`` of the removed item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If removal fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRemoveItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext) async throws -> FSRemoveItemResult

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an instance of ``FSGetAttributesResult`` containing the requested attributes, along with a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an instance of ``FSGetAttributesResult`` containing the requested attributes, along with a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext) async throws -> FSGetAttributesResult

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an instance of ``FSSetAttributesResult`` containing the item's updated attributes and the volume's updated free space, along with a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an instance of ``FSSetAttributesResult`` containing the item's updated attributes and the volume's updated free space, along with a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext) async throws -> FSSetAttributesResult

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass an instance of ``FSEnumerateDirectoryResult`` containing the current verifier, along with a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores the ``FSEnumerateDirectoryResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass an instance of ``FSEnumerateDirectoryResult`` containing the current verifier, along with a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores the ``FSEnumerateDirectoryResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext) async throws -> FSEnumerateDirectoryResult

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadSymlinkResult`` containing the link's contents and attributes, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func readSymbolicLink(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadSymlinkResult`` containing the link's contents and attributes, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func readSymbolicLink(_ item: FSItem, context: FSContext) async throws -> FSReadSymlinkResult
    }

    /// Flags to specify the policy when setting extended file attributes.
    @available(macOS 15.4, *)
    public enum SetXattrPolicy : UInt, @unchecked Sendable {

        /// Set the value, regardless of previous state.
        case alwaysSet = 0

        /// Set the value, but fail if the extended attribute already exists.
        case mustCreate = 1

        /// Set the value, but fail if the extended attribute doesn't already exist.
        case mustReplace = 2

        /// Delete the value, failing if the extended attribute doesn't exist.
        case delete = 3
    }

    /// Methods and properties implemented by volumes that natively or partially support extended attributes.
    ///
    /// > Deprecated: Use ``FSVolume/XattrHandler``
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol XattrOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var xattrOperationsInhibited: Bool { get set }

        /// Returns an array that specifies the extended attribute names the given item supports.
        ///
        /// If `item` supports no extended attributes, this method returns `nil`.
        ///
        /// Only implement this method if your volume works with "limited" extended attributes.
        /// For purposes of this protocol, "limited" support means the volume doesn't support extended attributes generally, but uses these APIs to expose specific file system data.
        ///
        /// > Note: If a file system implements this method, FSKit assumes limited support for extended attributes exists. In this mode, FSkit only calls this protocol's methods for the extended attribute names this method returns.
        ///
        /// - Parameter item: The item for which to get information.
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an data instance containing the extended attribute data and a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores any data in this case. For an `async` Swift implementation, there's no reply handler; simply return the data or throw an error.
        func getXattr(named name: FSFileName, of item: FSItem, replyHandler reply: @escaping @Sendable (Data?, (any Error)?) -> Void)

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an data instance containing the extended attribute data and a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores any data in this case. For an `async` Swift implementation, there's no reply handler; simply return the data or throw an error.
        func xattr(named name: FSFileName, of item: FSItem) async throws -> Data

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute fails, pass an error as the one parameter to the reply handler. If setting the attribute succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute fails, pass an error as the one parameter to the reply handler. If setting the attribute succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy) async throws

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass the xattrs as an array of ``FSFileName`` instances and a `nil` error. If getting the attributes fails, pass `nil` along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func listXattrs(of item: FSItem, replyHandler reply: @escaping @Sendable ([FSFileName]?, (any Error)?) -> Void)

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass the xattrs as an array of ``FSFileName`` instances and a `nil` error. If getting the attributes fails, pass `nil` along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func xattrs(of item: FSItem) async throws -> [FSFileName]
    }

    /// Methods and properties implemented by volumes that natively or partially support extended attributes.
    ///
    /// > Important: This protocol replaces the ``FSVolumeXattrOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol XattrHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var xattrOperationsInhibited: Bool { get }

        /// Returns an array that specifies the extended attribute names the given item supports.
        ///
        /// If `item` supports no extended attributes, this method returns `nil`.
        ///
        /// Only implement this method if your volume works with "limited" extended attributes.
        /// For purposes of this protocol, "limited" support means the volume doesn't support extended attributes generally, but uses these APIs to expose specific file system data.
        ///
        /// > Note: If a file system implements this method, FSKit assumes limited support for extended attributes exists. In this mode, FSkit only calls this protocol's methods for the extended attribute names this method returns.
        ///
        /// - Parameter item: The item for which to get information.
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an instance of ``FSGetXattrResult`` containing the extended attribute data, along with a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func getXattr(named name: FSFileName, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an instance of ``FSGetXattrResult`` containing the extended attribute data, along with a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func xattr(named name: FSFileName, of item: FSItem, context: FSContext) async throws -> FSGetXattrResult

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute succeeds, pass an instance of ``FSSetXattrResult`` containing the volume's updated free space, along with a `nil` error. If setting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute succeeds, pass an instance of ``FSSetXattrResult`` containing the volume's updated free space, along with a `nil` error. If setting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext) async throws -> FSSetXattrResult

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass an instance of ``FSListXattrsResult`` containing the xattrs as an array of ``FSFileName`` instances, along with a `nil` error. If getting the attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSListXattrsResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func listXattrs(of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass an instance of ``FSListXattrsResult`` containing the xattrs as an array of ``FSFileName`` instances, along with a `nil` error. If getting the attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSListXattrsResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func xattrs(of item: FSItem, context: FSContext) async throws -> FSListXattrsResult
    }

    /// Defined modes for opening a file.
    @available(macOS 15.4, *)
    public struct OpenModes : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// The read mode.
        ///
        /// This mode is equivalent to POSIX `FREAD`.
        public static var read: FSVolume.OpenModes { get }

        /// The write mode.
        ///
        /// This mode is equivalent to POSIX `FRWITE`.
        public static var write: FSVolume.OpenModes { get }
    }

    /// Methods and properties implemented by volumes that want to receive open and close calls for each item.
    ///
    /// When a file system volume conforms to this protocol, the kernel layer issues an open call to indicate desired access, and a close call to indicate what access to retain.
    /// A file is fully closed when the kernel layer issues a close call with no retained open nodes.
    /// When a file system receives the close call, it removes all access to the item.
    /// When all memory mappings to the item release, the kernel layer issues a final close.
    ///
    /// If a file system volume doesn't conform to this protocol, the kernel layer can skip making such calls to the volume.
    ///
    /// > Deprecated: Use ``FSVolume/OpenCloseHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol OpenCloseOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isOpenCloseInhibited: Bool { get set }

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
    }

    /// Methods and properties implemented by volumes that want to receive open and close calls for each item.
    ///
    /// When a file system volume conforms to this protocol, the kernel layer issues an open call to indicate desired access, and a close call to indicate what access to retain.
    /// A file is fully closed when the kernel layer issues a close call with no retained open nodes.
    /// When a file system receives the close call, it removes all access to the item.
    /// When all memory mappings to the item release, the kernel layer issues a final close.
    ///
    /// If a file system volume doesn't conform to this protocol, the kernel layer can skip making such calls to the volume.
    ///
    /// > Important: This protocol replaces the ``FSVolumeOpenCloseOperations`` protocol. It exposes the same functionality, while adding the ``FSContext`` parameters.
    @available(macOS 27.0, *)
    public protocol OpenCloseHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isOpenCloseInhibited: Bool { get }

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
    }

    /// Methods implemented for read and write operations that deliver data to and from the extension.
    ///
    /// Most volumes conform to either this protocol or ``FSVolumeKernelOffloadedIOOperations``.
    /// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
    /// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use this protocol, and those without it use ``FSVolumeKernelOffloadedIOOperations``.
    /// A volume that doesn't conform to either protocol can't support any I/O operation.
    ///
    /// > Deprecated: Use ``FSVolume/ReadWriteHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ReadWriteOperations : NSObjectProtocol {

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the number of bytes read and a `nil` error. If reading fails, pass the number of bytes read prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the number of bytes read and a `nil` error. If reading fails, pass the number of bytes read prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> Int

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass the number of bytes written and a `nil` error. If writing fails, pass the number of bytes written prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass the number of bytes written and a `nil` error. If writing fails, pass the number of bytes written prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int
    }

    /// Methods implemented for read and write operations that deliver data to and from the extension.
    ///
    /// Most volumes conform to either this protocol or ``FSVolumeKernelOffloadedIOHandler``.
    /// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
    /// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use this protocol, and those without it use ``FSVolumeKernelOffloadedIOHandler``.
    /// Set that attribute on an item when you create it; The kernel sets each file's routing when it first instantiates the item. This choice is immutable for the item's lifetime.
    /// A volume that doesn't conform to either protocol can't support any I/O operation.
    ///
    /// > Important: This protocol replaces the ``FSVolumeReadWriteOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol ReadWriteHandler : NSObjectProtocol {

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadFileResult`` containing the number of bytes read and the updated ``FSItemAttributes`` of the file, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadFileResult`` containing the number of bytes read and the updated ``FSItemAttributes`` of the file, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> FSReadFileResult

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass an instance of ``FSWriteFileResult`` containing the number of bytes written, the updated ``FSItemAttributes`` of the file, and the volume's updated free space, along with a `nil` error. If writing fails, pass the relevant error as the second parameter; FSKit ignores the ``FSWriteFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass an instance of ``FSWriteFileResult`` containing the number of bytes written, the updated ``FSItemAttributes`` of the file, and the volume's updated free space, along with a `nil` error. If writing fails, pass the relevant error as the second parameter; FSKit ignores the ``FSWriteFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> FSWriteFileResult
    }

    /// A bitmask of access rights.
    @available(macOS 15.4, *)
    public struct AccessMask : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// The file system allows reading data.
        public static var readData: FSVolume.AccessMask { get }

        /// The file system allows listing directory contents.
        public static var listDirectory: FSVolume.AccessMask { get }

        /// The file system allows writing data.
        public static var writeData: FSVolume.AccessMask { get }

        /// The file system allows adding files.
        public static var addFile: FSVolume.AccessMask { get }

        /// The file system allows file execution.
        public static var execute: FSVolume.AccessMask { get }

        /// The file system allows searching files.
        public static var search: FSVolume.AccessMask { get }

        /// The file system allows deleting a file.
        public static var delete: FSVolume.AccessMask { get }

        /// The file system allows appending data to a file.
        public static var appendData: FSVolume.AccessMask { get }

        /// The file system allows adding subdirectories.
        public static var addSubdirectory: FSVolume.AccessMask { get }

        /// The file system allows deleting subdirectories.
        public static var deleteChild: FSVolume.AccessMask { get }

        /// The file system allows reading file attributes.
        public static var readAttributes: FSVolume.AccessMask { get }

        /// The file system allows writing file attributes.
        public static var writeAttributes: FSVolume.AccessMask { get }

        /// The file system allows reading extended file attributes.
        public static var readXattr: FSVolume.AccessMask { get }

        /// The file system allows writing extended file attributes.
        public static var writeXattr: FSVolume.AccessMask { get }

        /// The file system allows reading a file's security descriptors.
        public static var readSecurity: FSVolume.AccessMask { get }

        /// The file system allows writing a file's security descriptors.
        public static var writeSecurity: FSVolume.AccessMask { get }

        /// The file system allows taking ownership of a file.
        public static var takeOwnership: FSVolume.AccessMask { get }
    }

    /// Methods and properties implemented by volumes that want to enforce access check operations.
    ///
    /// > Deprecated: Use ``FSVolume/AccessCheckHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol AccessCheckOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isAccessCheckInhibited: Bool { get set }

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass a Boolean value to indicate whether the file system grants access, followed by a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the Boolean parameter in this case. For an `async` Swift implementation, there's no reply handler; simply return the `Bool` or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, replyHandler reply: @escaping @Sendable (Bool, (any Error)?) -> Void)

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass a Boolean value to indicate whether the file system grants access, followed by a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the Boolean parameter in this case. For an `async` Swift implementation, there's no reply handler; simply return the `Bool` or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask) async throws -> Bool
    }

    /// Methods and properties implemented by volumes that want to enforce access check operations.
    ///
    /// > Important: This protocol replaces the ``FSVolumeAccessCheckOperations`` protocol. It exposes the same functionality, while using the ``FSCheckAccessResult`` object, to align with all other `Handler` protocols.
    @available(macOS 27.0, *)
    public protocol AccessCheckHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isAccessCheckInhibited: Bool { get }

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass an instance of ``FSCheckAccessResult`` containing a Boolean value to indicate whether the file system grants access, along with a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCheckAccessResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext, replyHandler reply: @escaping @Sendable (FSCheckAccessResult?, (any Error)?) -> Void)

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass an instance of ``FSCheckAccessResult`` containing a Boolean value to indicate whether the file system grants access, along with a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCheckAccessResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext) async throws -> FSCheckAccessResult
    }

    /// Methods and properties implemented by volumes that support renaming the volume.
    ///
    /// > Deprecated: Use ``FSVolume/RenameHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol RenameOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isVolumeRenameInhibited: Bool { get set }

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an ``FSFileName`` of the new volume name and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func setVolumeName(_ name: FSFileName, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an ``FSFileName`` of the new volume name and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func setVolumeName(_ name: FSFileName) async throws -> FSFileName
    }

    /// Methods and properties implemented by volumes that support renaming the volume.
    ///
    /// > Important: This protocol replaces the ``FSVolumeRenameOperations`` protocol. It exposes the same functionality, while using the ``FSVolumeRenameResult`` object, to align with all other `Handler` protocols.
    @available(macOS 27.0, *)
    public protocol RenameHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isVolumeRenameInhibited: Bool { get }

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSVolumeRenameResult`` containing the ``FSFileName`` of the new volume name, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSVolumeRenameResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setVolumeName(_ name: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSVolumeRenameResult?, (any Error)?) -> Void)

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSVolumeRenameResult`` containing the ``FSFileName`` of the new volume name, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSVolumeRenameResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setVolumeName(_ name: FSFileName, context: FSContext) async throws -> FSVolumeRenameResult
    }

    /// Behavior flags for preallocation operations.
    @available(macOS 15.4, *)
    public struct PreallocateFlags : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// Allocates contiguous space.
        public static var contiguous: FSVolume.PreallocateFlags { get }

        /// Allocates all requested space or no space at all.
        public static var all: FSVolume.PreallocateFlags { get }

        /// Allocates space that isn't freed when deleting the descriptor.
        ///
        /// This space remains allocated even after calling `close(2)`.
        public static var persist: FSVolume.PreallocateFlags { get }

        /// Allocates space from the physical end of file.
        ///
        /// When implementing this behavior, ignore any offset in the preallocate call.
        /// This flag is currently set for all ``FSVolume/PreallocateHandler/preallocateSpace(for:at:length:flags:context:replyHandler:)`` calls.
        public static var fromEOF: FSVolume.PreallocateFlags { get }
    }

    /// Methods and properties implemented by volumes that want to offer preallocation functions.
    ///
    /// A preallocation operation allocates space for a file without writing to it yet.
    /// A file system may use reallocation to avoid performing space allocation while in the midst of I/O; this strategy improves performance.
    /// Also, if the expected I/O pattern is many small writes, preallocating contiguous chunks may prevent fragmenting the file system.
    /// This process can improve performance later.
    ///
    /// In a kernel-based file system, you typically preallocate space with the `VNOP_ALLOCATE` operation, called from `fcntl(F_PREALLOCATE)`.
    ///
    /// > Deprecated: Use ``FSVolume/PreallocateHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol PreallocateOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isPreallocateInhibited: Bool { get set }

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there's no reply handler; simply return the allocated byte count or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there's no reply handler; simply return the allocated byte count or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags) async throws -> Int
    }

    /// Methods and properties implemented by volumes that want to offer preallocation functions.
    ///
    /// A preallocation operation allocates space for a file without writing to it yet.
    /// A file system may use reallocation to avoid performing space allocation while in the midst of I/O; this strategy improves performance.
    /// Also, if the expected I/O pattern is many small writes, preallocating contiguous chunks may prevent fragmenting the file system.
    /// This process can improve performance later.
    ///
    /// In a kernel-based file system, you typically preallocate space with the `VNOP_ALLOCATE` operation, called from `fcntl(F_PREALLOCATE)`.
    ///
    /// > Important: This protocol replaces the ``FSVolumePreallocateOperations`` protocol. It exposes the same functionality, while using the ``FSPreallocateResult`` object. This objects adds the ability to reply with ``FSItemAttributes`` and free space from ``preallocateSpace(for:at:length:flags:context:replyHandler:)``.
    @available(macOS 27.0, *)
    public protocol PreallocateHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isPreallocateInhibited: Bool { get }

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file and the volume's updated free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void)

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file and the volume's updated free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext) async throws -> FSPreallocateResult
    }

    /// Options to specify the item deactivation policy.
    ///
    /// Callers may want to set a deactivation policy because ``FSVolume/ItemDeactivation/deactivateItem(_:)`` processing blocks the kernel.
    /// Setting a deactivation policy allows the file system to take action at a definitive point in the item's life cycle.
    /// These options allow the file system to instruct the FSKit kernel of which circumstances require the expense of a round-trip call to the module.
    ///
    /// > Note: To avoid performing deactivation calls, Objective-C developers use the value `FSItemDeactivationNever`. In Swift, use an empty option set (`[]`).
    @available(macOS 15.4, *)
    public struct ItemDeactivationOptions : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// An option to always perform deactivation calls.
        ///
        /// Use this option if the file system needs `deactivateItem` calls in circumstances beyond those covered by ``forRemovedItems`` and ``forPreallocatedItems``.
        public static var always: FSVolume.ItemDeactivationOptions { get }

        /// An option to process deactivation for open-unlinked items at the moment of last close.
        public static var forRemovedItems: FSVolume.ItemDeactivationOptions { get }

        /// An option to process deactivation for for files with preallocated space.
        ///
        /// This option facilitates a sort of trim-on-close behavior.
        /// It is only meaningful for volumes that conform to ``FSVolume/PreallocateHandler``.
        public static var forPreallocatedItems: FSVolume.ItemDeactivationOptions { get }
    }

    /// Methods and properties implemented by volumes that support deactivating items.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ItemDeactivation : NSObjectProtocol {

        /// A property that tells FSKit to which types of items the deactivation applies, if any.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Operations/reclaimItem(_:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - reply: A block or closure to indicate success or failure. If deactivation fails, pass an error as the one parameter to the reply handler. If deactivation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Operations/reclaimItem(_:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - reply: A block or closure to indicate success or failure. If deactivation fails, pass an error as the one parameter to the reply handler. If deactivation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateItem(_ item: FSItem) async throws
    }

    /// Methods and properties implemented by volumes that support deactivating items.
    ///
    /// > Important: This protocol replaces the ``FSVolumeItemDeactivation`` protocol. It exposes the same functionality, while using the ``FSDeactivateItemResult`` object. This object adds the ability to reply with free space from ``deactivateItem(_:context:replyHandler:)``.
    @available(macOS 27.0, *)
    public protocol ItemDeactivationHandler : NSObjectProtocol {

        /// A property that tells FSKit to which types of items the deactivation applies, if any.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Handler/reclaimItem(_:replyHandler:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If deactivation succeeds, pass an instance of ``FSDeactivateItemResult`` containing the volume's updated free space, along with a `nil` error. If deactivation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSDeactivateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func deactivateItem(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSDeactivateItemResult?, (any Error)?) -> Void)

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Handler/reclaimItem(_:replyHandler:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If deactivation succeeds, pass an instance of ``FSDeactivateItemResult`` containing the volume's updated free space, along with a `nil` error. If deactivation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSDeactivateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func deactivateItem(_ item: FSItem, context: FSContext) async throws -> FSDeactivateItemResult
    }

    /// Types of region for seek operations
    @available(macOS 27.0, *)
    public enum SeekRegion : UInt, @unchecked Sendable {

        /// Seek the next hole region.
        ///
        /// When there are no more hole regions past the supplied `offset`,
        /// the current file size (end-of-file offset) should be returned.
        case hole = 1

        /// Seek the next data region.
        ///
        /// When there are no more data regions past the supplied `offset`,
        /// an error code `ENXIO` should be returned.
        case data = 2
    }

    /// Methods and properties implemented by volumes that support seek operations
    @available(macOS 27.0, *)
    public protocol SeekRegionHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isSeekRegionInhibited: Bool { get }

        /// Find the next offset of hole or data region greater than or equal to the
        /// supplied offset
        ///
        /// - Parameters:
        ///   - item: The item for which to seek.
        ///   - offset: The offset from which to seek.
        ///   - region: The region to seek.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If seek succeeds, pass an instance of ``FSSeekRegionResult`` containing the offset of the requested region greater than or equal to the supplied offset, along with a `nil` error. If seek fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSeekRegionResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext, replyHandler reply: @escaping @Sendable (FSSeekRegionResult?, (any Error)?) -> Void)

        /// Find the next offset of hole or data region greater than or equal to the
        /// supplied offset
        ///
        /// - Parameters:
        ///   - item: The item for which to seek.
        ///   - offset: The offset from which to seek.
        ///   - region: The region to seek.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If seek succeeds, pass an instance of ``FSSeekRegionResult`` containing the offset of the requested region greater than or equal to the supplied offset, along with a `nil` error. If seek fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSeekRegionResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext) async throws -> FSSeekRegionResult
    }

    /// A type that identifies a volume.
    ///
    /// For most volumes, the volume identifier is the UUID identifying the volume.
    ///
    /// Network file systems may access the same underlying volume using different authentication credentials.
    /// To handle this situation, add qualifying data to identify the specific container, as discussed in the superclass, ``FSEntityIdentifier``.
    ///
    /// > Important: Don't subclass this class.
    @available(macOS 15.4, *)
    open class Identifier : FSEntityIdentifier {
    }

    /// Mount options to be requested from FSKit using the `requestedMountOptions` property.
    @available(macOS 26.4, *)
    public struct MountOptions : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// An option to request a read-only mount.
        public static var readOnly: FSVolume.MountOptions { get }
    }

    /// An enumeration of case-sensitivity support types.
    ///
    /// A case-sensitive volume is a volume that treats upper and lower case characters in file and directory names as being distinct from each other. For example, `FILE.TXT` and `file.TXT` are different names in a case-sensitive volume, and the same name in a case-insensitive volume.
    @available(macOS 15.4, *)
    public enum CaseFormat : Int, @unchecked Sendable {

        /// The volume is case sensitive.
        case sensitive = 0

        /// The volume isn't case sensitive.
        case insensitive = 1

        /// The volume isn't case sensitive, but supports preserving the case of file and directory names.
        case insensitiveCasePreserving = 2
    }

    /// A type that represents capabilities supported by a volume, such as hard and symbolic links, journaling, and large file sizes.
    @available(macOS 15.4, *)
    open class SupportedCapabilities : NSObject, NSSecureCoding {

        /// A Boolean property that indicates whether the volume supports persistent object identifiers and can look up file system objects by their IDs.
        open var supportsPersistentObjectIDs: Bool

        /// A Boolean property that indicates whether the volume supports symbolic links.
        open var supportsSymbolicLinks: Bool

        /// A Boolean property that indicates whether the volume supports hard links.
        open var supportsHardLinks: Bool

        /// A Boolean property that indicates whether the volume supports a journal used to speed recovery in case of unplanned restart, such as a power outage or crash.
        ///
        /// This property doesn't necessarily mean the volume is actively using a journal.
        open var supportsJournal: Bool

        /// A Boolean property that indicates whether the volume currently uses a journal for speeding recovery after an unplanned shutdown.
        open var supportsActiveJournal: Bool

        /// A Boolean property that indicates the volume doesn't store reliable times for the root directory.
        ///
        /// If this value is `true` (Swift) or `YES` (Objective-C), the volume doesn't store reliable times for the root directory.
        open var doesNotSupportRootTimes: Bool

        /// A Boolean property that indicates whether the volume supports sparse files.
        ///
        /// A sparse file is a file that can have "holes" that the file system has never written to, and as a result don't consume space on disk.
        open var supportsSparseFiles: Bool

        /// A Boolean property that indicates whether the volume supports zero runs
        ///
        /// If this value is true, the volume keeps track of allocated but unwritten runs of a file so that it can substitute zeroes without actually writing zeroes to the media.
        open var supportsZeroRuns: Bool

        /// A Boolean property that indicates whether the volume supports fast results when fetching file system statistics.
        ///
        /// A true value means this volume hints to upper layers to indicate that `statfs(2)` is fast enough that its results need not be cached by the caller.
        open var supportsFastStatFS: Bool

        /// A Boolean property that indicates whether the volume supports file sizes larger than 4GB, and potentially up to 2TB.
        open var supports2TBFiles: Bool

        /// A Boolean property that indicates whether the volume supports open deny modes.
        ///
        /// These are modes such as "open for read write, deny write".
        open var supportsOpenDenyModes: Bool

        /// A Boolean property that indicates whether the volume supports hidden files.
        ///
        /// A `true` value means the volume supports the `UF_HIDDEN` file flag.
        open var supportsHiddenFiles: Bool

        /// A Boolean property that indicates the volume doesn't support certain volume size reports.
        ///
        /// A true value means the volume doesn't support determining values for total data blocks, available blocks, or free blocks, as in `f_blocks`, `f_bavail`, and `f_bfree` in the struct `statFS` returned by `statfs(2)`.
        open var doesNotSupportVolumeSizes: Bool

        /// A Boolean property that indicates whether the volume supports 64-bit object IDs.
        open var supports64BitObjectIDs: Bool

        /// A Boolean property that indicates whether the volume supports document IDs for document revisions.
        ///
        /// A document ID is an identifier that persists across object ID changes.
        open var supportsDocumentID: Bool

        /// A Boolean property that indicates the volume doesn't support immutable files.
        ///
        /// A `true` value means this volume doesn't support setting the `UF_IMMUTABLE` flag.
        open var doesNotSupportImmutableFiles: Bool

        /// A Boolean property that indicates the volume doesn't set file permissions.
        ///
        /// If this value is `true` (Swift) or `YES` (Objective-C), the volume doesn't support setting file permissions.
        open var doesNotSupportSettingFilePermissions: Bool

        /// A Boolean property that indicates whether the volume supports multiple logical file systems that share space in a single "partition."
        open var supportsSharedSpace: Bool

        /// A Boolean property that indicates whether the volume supports volume groups.
        ///
        /// Volume groups involve multiple logical file systems that the system can mount and unmount together, and for which the system can present common file system identifier information.
        open var supportsVolumeGroups: Bool

        /// A value that indicates the volume's support for case sensitivity.
        open var caseFormat: FSVolume.CaseFormat
    }
}

extension FSVolume {

    /// Properties implemented by volumes that support providing the values of system limits or options.
    ///
    /// This protocol gathers properties related to the `pathconf` and `fpathconf` system calls.
    ///
    /// For a file, the value of a property applies to just that file; for a directory, the value applies to all items in the directory.
    ///
    /// Properties that represent limits and have a numeric type use `-1` to represent no limit.
    @available(macOS 15.4, *)
    public protocol PathConfOperations : NSObjectProtocol {

        /// A property that represents the maximum number of hard links to the object.
        var maximumLinkCount: Int { get }

        /// A property that represents the maximum length of a component of a filename.
        var maximumNameLength: Int { get }

        /// A Boolean property that indicates whether the volume restricts ownership changes based on authorization.
        ///
        /// If this value is true, the volume rejects a `chown(2)` from anyone other than the superuser.
        var restrictsOwnershipChanges: Bool { get }

        /// A property that indicates whether the volume truncates files longer than its maximum supported length.
        ///
        /// If this value is `true`, the volume truncates the filename to ``maximumNameLength`` if the filename is longer than that.
        /// If this value is false, the file system responds with the error code `ENAMETOOLONG` if the filename is longer than ``maximumNameLength``.
        var truncatesLongNames: Bool { get }

        /// The maximum extended attribute size in bytes.
        ///
        /// Implement at least one of `maximumXattrSize` or ``maximumXattrSizeInBits``.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumXattrSizeInBits` implementation.
        optional var maximumXattrSize: Int { get }

        /// The maximum extended attribute size in bits.
        ///
        /// Implement at least one of ``maximumXattrSize`` or `maximumXattrSizeInBits`.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumXattrSizeInBits` implementation.
        optional var maximumXattrSizeInBits: Int { get }

        /// The maximum size of a regular file allowed in the volume.
        ///
        /// Implement at least one of `maximumFileSize` or ``maximumFileSizeInBits``.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumFileSizeInBits` implementation.
        optional var maximumFileSize: UInt64 { get }

        /// The minimum number of bits needed to represent, as a signed integer value, the maximum size of a regular file allowed in the volume.
        ///
        /// The maximum file size is `2^(maximumFileSizeInBits - 1)`.
        ///
        /// | Maximum file size (bytes)  | Maximum (in hex)     | Unsigned bits | Signed bits |
        /// | -------------------------: | -------------------: | ------------: | ----------: |
        /// |                     65,535 |             `0xFFFF` |            16 |          17 |
        /// |              2,147,483,647 |         `0x7FFFFFFF` |            31 |          32 |
        /// |              4,294,967,295 |         `0xFFFFFFFF` |            32 |          33 |
        /// | 18,446,744,073,709,551,615 | `0xFFFFFFFFFFFFFFFF` |            64 |          65 |
        ///
        /// Implement at least one of ``maximumFileSize`` or `maximumFileSizeInBits`.
        /// FSKit automatically converts from one to another if needed.
        /// If you implement both, FSKit uses only the `maximumFileSizeInBits` implementation.
        optional var maximumFileSizeInBits: Int { get }
    }

    /// Methods common to `FSVolumeHandler` and `FSVolumeOperations`
    @available(macOS 15.4, *)
    public protocol CommonOperations {

        /// A property that provides the supported capabilities of the volume.
        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }

        /// A property that provides up-to-date statistics of the volume.
        var volumeStatistics: FSStatFSResult { get }

        /// A property that allows the file system to use open-unlink emulation.
        ///
        /// _Open-unlink_ functionality refers to a file system's ability to support an open file being fully unlinked from the file system namespace.
        /// If a file system doesn't support this functionality, FSKit can emulate it instead; this is called "open-unlink emulation".
        ///
        /// Implement this property to return `true` (Swift) or `YES` (Objective-C) to allow FSKit to perform open-unlink emulation.
        /// If you don't implement this property at all, FSKit doesn't perform open-unlink emulation for this volume.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        @available(macOS 26.0, *)
        optional var enableOpenUnlinkEmulation: Bool { get }

        /// A property that allows the file system to request for specific mount options from FSKit.
        ///
        /// FSKit reads this value after the volume replies to the ``mount(options:)`` call.
        /// Changing the returned value during the runtime of the volume has no effect.
        @available(macOS 26.4, *)
        optional var requestedMountOptions: FSVolume.MountOptions { get }

        /// Mounts this volume, using the specified options.
        ///
        /// FSKit calls this method as a signal that some process is trying to mount this volume.
        /// Your file system receives a call to ``activate(options:)`` prior to receiving any mount calls.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the mount. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If mounting fails, pass an error as the one parameter to the reply handler. If mounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Mounts this volume, using the specified options.
        ///
        /// FSKit calls this method as a signal that some process is trying to mount this volume.
        /// Your file system receives a call to ``activate(options:)`` prior to receiving any mount calls.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the mount. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If mounting fails, pass an error as the one parameter to the reply handler. If mounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func mount(options: FSTaskOptions) async throws

        /// Unmounts this volume.
        ///
        /// Clear and flush all cached state in your implementation of this method.
        ///
        /// - Parameters:
        ///   - reply: A block or closure to indicate success or failure. If unmounting fails, pass an error as the one parameter to the reply handler. If unmounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func unmount(replyHandler reply: @escaping @Sendable () -> Void)

        /// Unmounts this volume.
        ///
        /// Clear and flush all cached state in your implementation of this method.
        ///
        /// - Parameters:
        ///   - reply: A block or closure to indicate success or failure. If unmounting fails, pass an error as the one parameter to the reply handler. If unmounting succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply return normally.
        func unmount() async

        /// Synchronizes the volume with its underlying resource.
        ///
        /// After calling this method, FSKit assumes that the volume has sent all pending I/O or metadata to its resource.
        ///
        /// - Parameters:
        ///   - flags: Timing flags, as defined in `mount.h.` These flags let the file system know whether to run the operation in a blocking or nonblocking fashion.
        ///   - reply: A block or closure to indicate success or failure. If synchronization fails, pass an error as the one parameter to the reply handler. If synchronization succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Synchronizes the volume with its underlying resource.
        ///
        /// After calling this method, FSKit assumes that the volume has sent all pending I/O or metadata to its resource.
        ///
        /// - Parameters:
        ///   - flags: Timing flags, as defined in `mount.h.` These flags let the file system know whether to run the operation in a blocking or nonblocking fashion.
        ///   - reply: A block or closure to indicate success or failure. If synchronization fails, pass an error as the one parameter to the reply handler. If synchronization succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func synchronize(flags: FSSyncFlags) async throws

        /// Reclaims an item, releasing any resources allocated for the item.
        ///
        /// FSKit guarantees that for every ``FSItem`` returned by the volume, a corresponding reclaim operation occurs after the upper layers no longer reference that item. To use this behavior, call ``FSItem/tryReclaim(_:)`` in your implementation of this method.
        ///
        /// > Note: Block device file systems may assess whether an underlying resource terminates before processing reclaim operations. On unary file systems, for example, the associated volumes unmount when such resources disconnect from the system. The unmount triggers a reclaiming of all items. Some implementations benefit greatly from short-circuiting in such cases. With a terminated resource, all I/O results in an error, making short-circuiting the most efficient response.
        ///
        /// - Parameters:
        ///   - item: The item to reclaim.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Reclaims an item, releasing any resources allocated for the item.
        ///
        /// FSKit guarantees that for every ``FSItem`` returned by the volume, a corresponding reclaim operation occurs after the upper layers no longer reference that item. To use this behavior, call ``FSItem/tryReclaim(_:)`` in your implementation of this method.
        ///
        /// > Note: Block device file systems may assess whether an underlying resource terminates before processing reclaim operations. On unary file systems, for example, the associated volumes unmount when such resources disconnect from the system. The unmount triggers a reclaiming of all items. Some implementations benefit greatly from short-circuiting in such cases. With a terminated resource, all I/O results in an error, making short-circuiting the most efficient response.
        ///
        /// - Parameters:
        ///   - item: The item to reclaim.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func reclaimItem(_ item: FSItem) async throws
    }

    /// Methods that all volumes implement to provide required capabilities.
    ///
    /// Conform to this protocol in your subclass of ``FSVolume``.
    /// To provide additional capabilities, conform to the other `FSVolume` operations protocols, such as ``FSVolumeOpenCloseOperations`` and ``FSVolumeReadWriteOperations``.
    ///
    /// > Note: This protocol extends ``FSVolumePathConfOperations``, so your volume implementation must also conform to that protocol.
    ///
    /// > Deprecated: Use ``FSVolume/Handler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol Operations : FSVolume.CommonOperations, FSVolume.PathConfOperations {

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass the root ``FSItem`` and a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. In Swift, `reply` takes only the ``FSItem`` as the parameter; you signal any error with a `throw`. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func activate(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSItem?, (any Error)?) -> Void)

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass the root ``FSItem`` and a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. In Swift, `reply` takes only the ``FSItem`` as the parameter; you signal any error with a `throw`. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func activate(options: FSTaskOptions) async throws -> FSItem

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount()`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivate(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount()`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivate(options: FSDeactivateOptions = []) async throws

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName`` (as saved within the file system), along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; any ``FSItem`` or ``FSFileName`` are ignored in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` and ``FSFileName`` as a tuple or throw an error.
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName`` (as saved within the file system), along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; any ``FSItem`` or ``FSFileName`` are ignored in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` and ``FSFileName`` as a tuple or throw an error.
        func lookupItem(named name: FSFileName, inDirectory directory: FSItem) async throws -> (FSItem, FSFileName)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest) async throws -> (FSItem, FSFileName)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly-created ``FSItem`` and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItem`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItem`` or throw an error.
        func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName) async throws -> (FSItem, FSFileName)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an ``FSFileName`` of the newly-created link and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an ``FSFileName`` of the newly-created link and a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem) async throws -> FSFileName

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:)``. After doing so, ensure the operation finishes without errors.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass the ``FSFileName`` as it exists within `destinationDirectory` and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:)``. After doing so, ensure the operation finishes without errors.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass the ``FSFileName`` as it exists within `destinationDirectory` and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?) async throws -> FSFileName

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - reply: A block or closure to indicate success or failure. If removal fails, pass an error as the one parameter to the reply handler. If removal succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem) async throws

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an ``FSItemAttributes`` with the requested attributes populated and a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an ``FSItemAttributes`` with the requested attributes populated and a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem) async throws -> FSItem.Attributes

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an ``FSItemAttributes`` with the item's updated attributes and a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, replyHandler reply: @escaping @Sendable (FSItem.Attributes?, (any Error)?) -> Void)

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an ``FSItemAttributes`` with the item's updated attributes and a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores any ``FSItemAttributes`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSItemAttributes`` or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem) async throws -> FSItem.Attributes

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass the current verifier and a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores any verifier in this case. For an `async` Swift implementation, there's no reply handler; simply return the current verifier or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, replyHandler reply: @escaping @Sendable (FSDirectoryVerifier, (any Error)?) -> Void)

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass the current verifier and a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores any verifier in this case. For an `async` Swift implementation, there's no reply handler; simply return the current verifier or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker) async throws -> FSDirectoryVerifier

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the link's contents as an ``FSFileName`` and a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func readSymbolicLink(_ item: FSItem, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the link's contents as an ``FSFileName`` and a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func readSymbolicLink(_ item: FSItem) async throws -> FSFileName
    }

    /// Methods that all volumes implement to provide required capabilities.
    ///
    /// Conform to this protocol in your subclass of ``FSVolume``.
    /// To provide additional capabilities, conform to the other `FSVolume` handler protocols, such as ``FSVolumeOpenCloseHandler`` and ``FSVolumeReadWriteHandler``.
    ///
    /// > Note: This protocol extends ``FSVolumePathConfOperations``, so your volume implementation must also conform to that protocol.
    ///
    /// > Important: This protocol replaces the ``FSVolumeOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol Handler : FSVolume.CommonOperations, FSVolume.PathConfOperations {

        var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { get }

        /// A property that provides up-to-date statistics of the volume.
        var volumeStatistics: FSStatFSResult { get }

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass an instance of ``FSActivateResult`` containing the root ``FSItem``, along with a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSActivateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void)

        /// Activates the volume using the specified options.
        ///
        /// When FSKit calls this method, allocate any in-memory state required to represent the file system.
        /// Also allocate an ``FSItem`` for the root directory of the file system, and pass it to the reply block.
        /// FSKit caches this root item for the lifetime of the volume, and uses it as a starting point for all file look-ups.
        ///
        /// Volume activation occurs prior to any call to mount the volume.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the activation. These can include security-scoped file paths. There are no defined options currently.
        ///   - reply: A block or closure to indicate success or failure. If activation succeeds, pass an instance of ``FSActivateResult`` containing the root ``FSItem``, along with a `nil` error. If activation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSActivateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func activateVolume(options: FSTaskOptions) async throws -> FSActivateResult

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount(replyHandler:)`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateVolume(options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Tears down a previously initialized volume instance.
        ///
        /// Set up your implementation to release any resources allocated for the volume instance.
        /// By the time you receive this callback, FSKit has already performed a reclaim call to release all other file nodes associated with this file system instance.
        ///
        /// Avoid performing any I/O in this method.
        /// Prior to calling this method, FSKit has already issued a sync call to perform any
        /// cleanup-related I/O.
        ///
        /// FSKit unmounts any mounted volume with a call to ``unmount(replyHandler:)`` prior to the deactivate callback.
        ///
        /// - Parameters:
        ///   - options: Options to apply to the deactivation.
        ///   - reply: A block or closure to indicate success or failure. If activation fails, pass an error as the one parameter to the reply handler. If activation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateVolume(options: FSDeactivateOptions = []) async throws

        func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func mount(options: FSTaskOptions) async throws

        func unmount(replyHandler reply: @escaping @Sendable () -> Void)

        func unmount() async

        func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func synchronize(flags: FSSyncFlags) async throws

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemResult`` containing the found ``FSItem``, its ``FSFileName`` (as saved within the file system), and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void)

        /// Looks up an item within a directory.
        ///
        /// If no item matching `name` exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOENT`.
        ///
        /// > Tip: The ``FSFileName`` sent back to the caller may differ from the `name` parameter. This flexibility allows your implementation to handle case-insensitive and case-sensitive file systems. It might also be the case that `name` uses a composed Unicode string, but the name maintained by the file system and provided to the caller is uncomposed Unicode.
        ///
        /// - Parameters:
        ///   - name: The name of the item to look up.
        ///   - directory: The directory in which to look up the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemResult`` containing the found ``FSItem``, its ``FSFileName`` (as saved within the file system), and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSLookupItemResult

        func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        func reclaimItem(_ item: FSItem) async throws

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateItemResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void)

        /// Creates a new file or directory item.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - type: The new item's type.  Valid values are ``FSItem/ItemType/file`` or ``FSItem/ItemType/directory``.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateItemResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext) async throws -> FSCreateItemResult

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateSymlinkResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void)

        /// Creates a new symbolic link.
        ///
        /// If an item named `name` already exists in the directory indicated by `directory`, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `EEXIST`.
        ///
        /// - Parameters:
        ///   - name: The new item's name.
        ///   - directory: The directory in which to create the item.
        ///   - newAttributes: Attributes to apply to the new item.
        ///   - contents: The contents of the new symbolic link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateSymlinkResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext) async throws -> FSCreateSymlinkResult

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateLinkResult`` containing the ``FSFileName`` of the newly-created link, the ``FSItemAttributes`` of the linked item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateLinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void)

        /// Creates a new hard link.
        ///
        /// If creating the link fails, complete the request with an error with a domain of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and the following error codes:
        ///
        /// * `EEXIST` if there's already an item named `name` in the directory.
        /// * `EMLINK` if creating the link would exceed the maximum number of hard links supported on `item`.
        /// * `ENOTSUP` if the file system doesn't support creating hard links to the type of file system object that `item` represents.
        ///
        /// - Parameters:
        ///   - item: The existing item to which to link.
        ///   - name: The name for the new link.
        ///   - directory: The directory in which to create the link.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateLinkResult`` containing the ``FSFileName`` of the newly-created link, the ``FSItemAttributes`` of the linked item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateLinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext) async throws -> FSCreateLinkResult

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:replyHandler:)``. After doing so, ensure the operation finishes without errors.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSRenameItemResult`` containing the ``FSFileName`` as it exists within `destinationDirectory`, the ``FSItemAttributes`` of the renamed item, the updated ``FSItemAttributes`` of the source directory, the updated ``FSItemAttributes`` of the destination directory, the ``FSItemAttributes`` of the overwritten item (if any), and the volume's updated free space, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRenameItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void)

        /// Renames an item from one path in the file system to another.
        ///
        /// > Note: This method only implements plain `rename(2)` semantics. FSKit fails `renamex_np(2)` calls that pass flags, such as `RENAME_SWAP` or `RENAME_EXCL`, with `ENOTSUP` without calling this method.
        ///
        /// Implement renaming along the lines of this algorithm:
        ///
        /// - If `item` is a file:
        ///   - If the destination file exists:
        ///     - Remove the destination file.
        ///   - If the source and destination directories are the same:
        ///     - Rewrite the name in the existing directory.
        ///   - Else:
        ///     - Write the new entry in the destination directory.
        ///     - Clear the old directory entry.
        /// - If `item` is a directory:
        ///   - If the destination directory exists:
        ///     - If the destination directory isn't empty:
        ///       - Fail the operation with an error of <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and a code of `ENOTEMPTY`.
        ///     - Else:
        ///        - Remove the destination directory.
        ///     - If the source and destination directories are the same:
        ///       - Rewrite the name in the existing directory.
        ///     - Else:
        ///       - If the destination is a child of the source directory:
        ///         - Fail the operation with an error.
        ///       - Else:
        ///         - Write the new entry in the destination directory.
        ///         - Update `"."` and `".."` in the moved directory.
        ///         - Clear the old directory entry.
        ///
        /// - Parameters:
        ///   - item: The file system object being renamed.
        ///   - sourceDirectory: The directory that currently contains the item to rename.
        ///   - sourceName: The name of the item within the source directory.
        ///   - destinationName: The new name of the item as it appears in `destinationDirectory`.
        ///   - destinationDirectory: The directory to contain the renamed object, which may be the same as `sourceDirectory`.
        ///   - overItem: The file system object if the destination exists, as discovered in a prior lookup. If this parameter is non-`nil`, mark `overItem` as deleted, so the file system can free its allocated space on the next call to ``reclaimItem(_:replyHandler:)``. After doing so, ensure the operation finishes without errors.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSRenameItemResult`` containing the ``FSFileName`` as it exists within `destinationDirectory`, the ``FSItemAttributes`` of the renamed item, the updated ``FSItemAttributes`` of the source directory, the updated ``FSItemAttributes`` of the destination directory, the ``FSItemAttributes`` of the overwritten item (if any), and the volume's updated free space, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRenameItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext) async throws -> FSRenameItemResult

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If removal succeeds, pass an instance of ``FSRemoveItemResult`` containing the ``FSItemAttributes`` of the removed item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If removal fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRemoveItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void)

        /// Removes an existing item from a given directory.
        ///
        /// Don't actually remove the item object itself in your implementation; instead, only remove the given item name from the given directory.
        /// Remove and deallocate the item in ``reclaimItem(_:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - item: The item to remove.
        ///   - name: The name of the item to remove.
        ///   - directory: The directory from which to remove the item.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If removal succeeds, pass an instance of ``FSRemoveItemResult`` containing the ``FSItemAttributes`` of the removed item, the updated ``FSItemAttributes`` of the parent directory, and the volume's updated free space, along with a `nil` error. If removal fails, pass the relevant error as the second parameter; FSKit ignores the ``FSRemoveItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext) async throws -> FSRemoveItemResult

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an instance of ``FSGetAttributesResult`` containing the requested attributes, along with a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void)

        /// Fetches attributes for the given item.
        ///
        /// For file systems that don't support hard links, set ``FSItemAttributes/linkCount`` to `1` for regular files and symbolic links.
        ///
        /// If the item's `bsdFlags` contain the `UF_COMPRESSED` flag, your file system returns the uncompressed size of the file.
        ///
        /// - Parameters:
        ///   - desiredAttributes: A requested set of attributes to get. The implementation inspects the request's ``FSItemGetAttributesRequest/wantedAttributes`` to determine which attributes to populate.
        ///   - item: The item to get attributes for.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting attributes succeeds, pass an instance of ``FSGetAttributesResult`` containing the requested attributes, along with a `nil` error. If getting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext) async throws -> FSGetAttributesResult

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an instance of ``FSSetAttributesResult`` containing the item's updated attributes and the volume's updated free space, along with a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void)

        /// Sets the given attributes on an item.
        ///
        /// Several attributes are considered "read-only", and an attempt to set these attributes results in an error with the code `EINVAL`.
        ///
        /// A request may set ``FSItem/Attributes/size`` beyond the end of the file.
        /// If the underlying file system doesn't support sparse files, allocate space to fill the new file size.
        /// Either fill this space with zeroes, or configure it to read as zeroes.
        ///
        /// If a request sets the file size below the current end-of-file, truncate the file and return any unused space to the file system as free space.
        ///
        /// Ignore attempts to set the size of directories or symbolic links; don't produce an error.
        ///
        /// If the caller attempts to set an attribute not supported by the on-disk file system format, don't produce an error.
        /// The upper layers of the framework will detect this situation.
        ///
        /// - Parameters:
        ///   - newAttributes: A request containing the attributes to set.
        ///   - item: The item on which to set the attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting attributes succeeds, pass an instance of ``FSSetAttributesResult`` containing the item's updated attributes and the volume's updated free space, along with a `nil` error. If setting attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetAttributesResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext) async throws -> FSSetAttributesResult

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass an instance of ``FSEnumerateDirectoryResult`` containing the current verifier, along with a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores the ``FSEnumerateDirectoryResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void)

        /// Enumerates the contents of the given directory.
        ///
        /// This method uses the ``FSDirectoryEntryPacker/packEntry(name:itemType:itemID:nextCookie:attributes:)`` method of the `packer` parameter to deliver the enumerated items to the caller.
        /// The general flow of an enumeration implementation follows these steps:
        ///
        /// 1. Enumeration starts with a call to `enumerateDirectory` using the initial next-cookie and verifier values ``FSDirectoryCookieInitial`` and ``FSDirectoryVerifierInitial``, respectively.
        /// 2. The implementation uses `packer` to pack the initial set of directory entries. Packing also sets a `nextCookie` to use on the next call.
        /// 3. The implementation replies with a new verifier value, a nonzero value that reflects the directory's current version.
        /// 4. On the next call the implementation packs the next set of entries, starting with the item indicated by `cookie`. If `cookie` doesn't resolve to a valid directory entry, complete the request with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code ``FSError/Code/invalidDirectoryCookie``.
        ///
        /// When packing, make sure to use acceptable directory entry names and unambiguous input to all file operations that take names without additional normalization, such as`lookupName`.
        ///
        /// > Tip: If the `attributes` parameter is `nil`, include at least two entries in a directory: `"."` and `".."`, which represent the current and parent directories, respectively. Both of these items have type ``FSItem/ItemType/directory``. For the root directory, `"."` and `".."` have identical contents. Don't pack  `"."` and `".."` if `attributes` isn't `nil`.
        ///
        /// - Parameters:
        ///   - directory: The item to enumerate. FSKit guarantees this item is of type ``FSItem/ItemType/directory``.
        ///   - cookie: A value that indicates the location within the directory from which to enumerate. Your implementation defines the semantics of the cookie values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryCookieInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `nextCookie` parameter.
        ///   - verifier: A tool to detect whether the directory contents changed since the last call to `enumerateDirectory`. Your implementation defines the semantics of the verifier values; they're opaque to FSKit. The first call to the enumerate method passes ``FSDirectoryVerifierInitial`` for this parameter. Subsequent calls pass whatever cookie value you previously passed to the packer's `currentVerifier` parameter.
        ///   - attributes: The desired attributes to provide, or `nil` if the caller doesn't require attributes.
        ///   - packer: An object that your implementation uses to enumerate directory items, packing one item per callback to `enumerateDirectory`.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If enumeration succeeds, pass an instance of ``FSEnumerateDirectoryResult`` containing the current verifier, along with a `nil` error. If enumeration fails, pass the relevant error as the second parameter; FSKit ignores the ``FSEnumerateDirectoryResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext) async throws -> FSEnumerateDirectoryResult

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadSymlinkResult`` containing the link's contents and attributes, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func readSymbolicLink(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void)

        /// Reads a symbolic link.
        ///
        /// - Parameters:
        ///   - item: The symbolic link to read from. FSKit guarantees this item is of type ``FSItem/ItemType/symlink``.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadSymlinkResult`` containing the link's contents and attributes, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadSymlinkResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func readSymbolicLink(_ item: FSItem, context: FSContext) async throws -> FSReadSymlinkResult
    }

    /// Flags to specify the policy when setting extended file attributes.
    @available(macOS 15.4, *)
    public enum SetXattrPolicy : UInt, @unchecked Sendable {

        /// Set the value, regardless of previous state.
        case alwaysSet = 0

        /// Set the value, but fail if the extended attribute already exists.
        case mustCreate = 1

        /// Set the value, but fail if the extended attribute doesn't already exist.
        case mustReplace = 2

        /// Delete the value, failing if the extended attribute doesn't exist.
        case delete = 3
    }

    /// Methods and properties implemented by volumes that natively or partially support extended attributes.
    ///
    /// > Deprecated: Use ``FSVolume/XattrHandler``
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol XattrOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var xattrOperationsInhibited: Bool { get set }

        /// Returns an array that specifies the extended attribute names the given item supports.
        ///
        /// If `item` supports no extended attributes, this method returns `nil`.
        ///
        /// Only implement this method if your volume works with "limited" extended attributes.
        /// For purposes of this protocol, "limited" support means the volume doesn't support extended attributes generally, but uses these APIs to expose specific file system data.
        ///
        /// > Note: If a file system implements this method, FSKit assumes limited support for extended attributes exists. In this mode, FSkit only calls this protocol's methods for the extended attribute names this method returns.
        ///
        /// - Parameter item: The item for which to get information.
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an data instance containing the extended attribute data and a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores any data in this case. For an `async` Swift implementation, there's no reply handler; simply return the data or throw an error.
        func getXattr(named name: FSFileName, of item: FSItem, replyHandler reply: @escaping @Sendable (Data?, (any Error)?) -> Void)

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an data instance containing the extended attribute data and a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores any data in this case. For an `async` Swift implementation, there's no reply handler; simply return the data or throw an error.
        func xattr(named name: FSFileName, of item: FSItem) async throws -> Data

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute fails, pass an error as the one parameter to the reply handler. If setting the attribute succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute fails, pass an error as the one parameter to the reply handler. If setting the attribute succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy) async throws

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass the xattrs as an array of ``FSFileName`` instances and a `nil` error. If getting the attributes fails, pass `nil` along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func listXattrs(of item: FSItem, replyHandler reply: @escaping @Sendable ([FSFileName]?, (any Error)?) -> Void)

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass the xattrs as an array of ``FSFileName`` instances and a `nil` error. If getting the attributes fails, pass `nil` along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func xattrs(of item: FSItem) async throws -> [FSFileName]
    }

    /// Methods and properties implemented by volumes that natively or partially support extended attributes.
    ///
    /// > Important: This protocol replaces the ``FSVolumeXattrOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol XattrHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var xattrOperationsInhibited: Bool { get }

        /// Returns an array that specifies the extended attribute names the given item supports.
        ///
        /// If `item` supports no extended attributes, this method returns `nil`.
        ///
        /// Only implement this method if your volume works with "limited" extended attributes.
        /// For purposes of this protocol, "limited" support means the volume doesn't support extended attributes generally, but uses these APIs to expose specific file system data.
        ///
        /// > Note: If a file system implements this method, FSKit assumes limited support for extended attributes exists. In this mode, FSkit only calls this protocol's methods for the extended attribute names this method returns.
        ///
        /// - Parameter item: The item for which to get information.
        optional func supportedXattrNames(for item: FSItem) -> [FSFileName]

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an instance of ``FSGetXattrResult`` containing the extended attribute data, along with a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func getXattr(named name: FSFileName, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void)

        /// Gets the specified extended attribute of the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - item: The item for which to get the extended attribute.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the attribute succeeds, pass an instance of ``FSGetXattrResult`` containing the extended attribute data, along with a `nil` error. If getting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSGetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func xattr(named name: FSFileName, of item: FSItem, context: FSContext) async throws -> FSGetXattrResult

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute succeeds, pass an instance of ``FSSetXattrResult`` containing the volume's updated free space, along with a `nil` error. If setting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void)

        /// Sets the specified extended attribute data on the given item.
        ///
        /// - Parameters:
        ///   - name: The extended attribute name.
        ///   - value: The extended attribute value to set. This can't be `nil`, unless the policy is ``FSVolume/SetXattrPolicy/delete``.
        ///   - item: The item on which to set the extended attribute.
        ///   - policy: The policy to apply when setting the attribute. See ``FSSetXattrPolicy`` for possible values.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If setting the attribute succeeds, pass an instance of ``FSSetXattrResult`` containing the volume's updated free space, along with a `nil` error. If setting the attribute fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSetXattrResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext) async throws -> FSSetXattrResult

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass an instance of ``FSListXattrsResult`` containing the xattrs as an array of ``FSFileName`` instances, along with a `nil` error. If getting the attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSListXattrsResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func listXattrs(of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void)

        /// Gets the list of extended attributes currently set on the given item.
        ///
        /// - Parameters:
        ///   - item: The item from which to get extended attributes.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If getting the list of extended attributes succeeds, pass an instance of ``FSListXattrsResult`` containing the xattrs as an array of ``FSFileName`` instances, along with a `nil` error. If getting the attributes fails, pass the relevant error as the second parameter; FSKit ignores the ``FSListXattrsResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func xattrs(of item: FSItem, context: FSContext) async throws -> FSListXattrsResult
    }

    /// Defined modes for opening a file.
    @available(macOS 15.4, *)
    public struct OpenModes : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// The read mode.
        ///
        /// This mode is equivalent to POSIX `FREAD`.
        public static var read: FSVolume.OpenModes { get }

        /// The write mode.
        ///
        /// This mode is equivalent to POSIX `FRWITE`.
        public static var write: FSVolume.OpenModes { get }
    }

    /// Methods and properties implemented by volumes that want to receive open and close calls for each item.
    ///
    /// When a file system volume conforms to this protocol, the kernel layer issues an open call to indicate desired access, and a close call to indicate what access to retain.
    /// A file is fully closed when the kernel layer issues a close call with no retained open nodes.
    /// When a file system receives the close call, it removes all access to the item.
    /// When all memory mappings to the item release, the kernel layer issues a final close.
    ///
    /// If a file system volume doesn't conform to this protocol, the kernel layer can skip making such calls to the volume.
    ///
    /// > Deprecated: Use ``FSVolume/OpenCloseHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol OpenCloseOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isOpenCloseInhibited: Bool { get set }

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws
    }

    /// Methods and properties implemented by volumes that want to receive open and close calls for each item.
    ///
    /// When a file system volume conforms to this protocol, the kernel layer issues an open call to indicate desired access, and a close call to indicate what access to retain.
    /// A file is fully closed when the kernel layer issues a close call with no retained open nodes.
    /// When a file system receives the close call, it removes all access to the item.
    /// When all memory mappings to the item release, the kernel layer issues a final close.
    ///
    /// If a file system volume doesn't conform to this protocol, the kernel layer can skip making such calls to the volume.
    ///
    /// > Important: This protocol replaces the ``FSVolumeOpenCloseOperations`` protocol. It exposes the same functionality, while adding the ``FSContext`` parameters.
    @available(macOS 27.0, *)
    public protocol OpenCloseHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isOpenCloseInhibited: Bool { get }

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Opens a file for access.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The set of mode flags to open the item with.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening fails, pass an error as the one parameter to the reply handler. If opening succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func openItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Closes a file from further access.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - modes: The set of mode flags to keep after this close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If closing fails, pass an error as the one parameter to the reply handler. If closing succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, context: FSContext) async throws
    }

    /// Methods implemented for read and write operations that deliver data to and from the extension.
    ///
    /// Most volumes conform to either this protocol or ``FSVolumeKernelOffloadedIOOperations``.
    /// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
    /// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use this protocol, and those without it use ``FSVolumeKernelOffloadedIOOperations``.
    /// A volume that doesn't conform to either protocol can't support any I/O operation.
    ///
    /// > Deprecated: Use ``FSVolume/ReadWriteHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ReadWriteOperations : NSObjectProtocol {

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the number of bytes read and a `nil` error. If reading fails, pass the number of bytes read prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass the number of bytes read and a `nil` error. If reading fails, pass the number of bytes read prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> Int

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass the number of bytes written and a `nil` error. If writing fails, pass the number of bytes written prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass the number of bytes written and a `nil` error. If writing fails, pass the number of bytes written prior to the error along with the relevant error. For an `async` Swift implementation, there's no reply handler; simply return the byte count or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int
    }

    /// Methods implemented for read and write operations that deliver data to and from the extension.
    ///
    /// Most volumes conform to either this protocol or ``FSVolumeKernelOffloadedIOHandler``.
    /// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
    /// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use this protocol, and those without it use ``FSVolumeKernelOffloadedIOHandler``.
    /// Set that attribute on an item when you create it; The kernel sets each file's routing when it first instantiates the item. This choice is immutable for the item's lifetime.
    /// A volume that doesn't conform to either protocol can't support any I/O operation.
    ///
    /// > Important: This protocol replaces the ``FSVolumeReadWriteOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol ReadWriteHandler : NSObjectProtocol {

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadFileResult`` containing the number of bytes read and the updated ``FSItemAttributes`` of the file, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void)

        /// Reads the contents of the given file item.
        ///
        /// If the number of bytes requested exceeds the number of bytes available before the end of the file, then the call copies only those bytes to `buffer`.
        /// If `offset` points past the last valid byte of the file, don't reply with an error but set `actuallyRead` to `0`.
        ///
        /// - Parameters:
        ///   - item: The item from which to read. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start reading.
        ///   - length: The number of bytes to read.
        ///   - buffer: A buffer to receive the bytes read from the file.
        ///   - reply: A block or closure to indicate success or failure. If reading succeeds, pass an instance of ``FSReadFileResult`` containing the number of bytes read and the updated ``FSItemAttributes`` of the file, along with a `nil` error. If reading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSReadFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> FSReadFileResult

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass an instance of ``FSWriteFileResult`` containing the number of bytes written, the updated ``FSItemAttributes`` of the file, and the volume's updated free space, along with a `nil` error. If writing fails, pass the relevant error as the second parameter; FSKit ignores the ``FSWriteFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void)

        /// Writes contents to the given file item.
        ///
        /// FSKit expects this routine to allocate space in the file system to extend the file as necessary.
        ///
        /// If the volume experiences an out-of-space condition, reply with an error of domain <doc://com.apple.documentation/documentation/Foundation/NSPOSIXErrorDomain> and code `ENOSPC`.
        ///
        /// - Parameters:
        ///   - contents: A buffer containing the data to write to the file.
        ///   - item: The item to which to write. FSKit guarantees this item will be of type ``FSItem/ItemType/file``.
        ///   - offset: The offset in the file from which to start writing.
        ///   - reply: A block or closure to indicate success or failure. If writing succeeds, pass an instance of ``FSWriteFileResult`` containing the number of bytes written, the updated ``FSItemAttributes`` of the file, and the volume's updated free space, along with a `nil` error. If writing fails, pass the relevant error as the second parameter; FSKit ignores the ``FSWriteFileResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> FSWriteFileResult
    }

    /// A bitmask of access rights.
    @available(macOS 15.4, *)
    public struct AccessMask : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// The file system allows reading data.
        public static var readData: FSVolume.AccessMask { get }

        /// The file system allows listing directory contents.
        public static var listDirectory: FSVolume.AccessMask { get }

        /// The file system allows writing data.
        public static var writeData: FSVolume.AccessMask { get }

        /// The file system allows adding files.
        public static var addFile: FSVolume.AccessMask { get }

        /// The file system allows file execution.
        public static var execute: FSVolume.AccessMask { get }

        /// The file system allows searching files.
        public static var search: FSVolume.AccessMask { get }

        /// The file system allows deleting a file.
        public static var delete: FSVolume.AccessMask { get }

        /// The file system allows appending data to a file.
        public static var appendData: FSVolume.AccessMask { get }

        /// The file system allows adding subdirectories.
        public static var addSubdirectory: FSVolume.AccessMask { get }

        /// The file system allows deleting subdirectories.
        public static var deleteChild: FSVolume.AccessMask { get }

        /// The file system allows reading file attributes.
        public static var readAttributes: FSVolume.AccessMask { get }

        /// The file system allows writing file attributes.
        public static var writeAttributes: FSVolume.AccessMask { get }

        /// The file system allows reading extended file attributes.
        public static var readXattr: FSVolume.AccessMask { get }

        /// The file system allows writing extended file attributes.
        public static var writeXattr: FSVolume.AccessMask { get }

        /// The file system allows reading a file's security descriptors.
        public static var readSecurity: FSVolume.AccessMask { get }

        /// The file system allows writing a file's security descriptors.
        public static var writeSecurity: FSVolume.AccessMask { get }

        /// The file system allows taking ownership of a file.
        public static var takeOwnership: FSVolume.AccessMask { get }
    }

    /// Methods and properties implemented by volumes that want to enforce access check operations.
    ///
    /// > Deprecated: Use ``FSVolume/AccessCheckHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol AccessCheckOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isAccessCheckInhibited: Bool { get set }

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass a Boolean value to indicate whether the file system grants access, followed by a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the Boolean parameter in this case. For an `async` Swift implementation, there's no reply handler; simply return the `Bool` or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, replyHandler reply: @escaping @Sendable (Bool, (any Error)?) -> Void)

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass a Boolean value to indicate whether the file system grants access, followed by a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the Boolean parameter in this case. For an `async` Swift implementation, there's no reply handler; simply return the `Bool` or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask) async throws -> Bool
    }

    /// Methods and properties implemented by volumes that want to enforce access check operations.
    ///
    /// > Important: This protocol replaces the ``FSVolumeAccessCheckOperations`` protocol. It exposes the same functionality, while using the ``FSCheckAccessResult`` object, to align with all other `Handler` protocols.
    @available(macOS 27.0, *)
    public protocol AccessCheckHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isAccessCheckInhibited: Bool { get }

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass an instance of ``FSCheckAccessResult`` containing a Boolean value to indicate whether the file system grants access, along with a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCheckAccessResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext, replyHandler reply: @escaping @Sendable (FSCheckAccessResult?, (any Error)?) -> Void)

        /// Checks whether the file system allows access to the given item.
        ///
        /// - Parameters:
        ///   - theItem: The item for which to check access.
        ///   - access: A mask indicating a set of access types for which to check.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If the access check succeeds, pass an instance of ``FSCheckAccessResult`` containing a Boolean value to indicate whether the file system grants access, along with a `nil` error. If the access check fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCheckAccessResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func checkAccess(to theItem: FSItem, requestedAccess access: FSVolume.AccessMask, context: FSContext) async throws -> FSCheckAccessResult
    }

    /// Methods and properties implemented by volumes that support renaming the volume.
    ///
    /// > Deprecated: Use ``FSVolume/RenameHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol RenameOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isVolumeRenameInhibited: Bool { get set }

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an ``FSFileName`` of the new volume name and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func setVolumeName(_ name: FSFileName, replyHandler reply: @escaping @Sendable (FSFileName?, (any Error)?) -> Void)

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an ``FSFileName`` of the new volume name and a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores any ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; simply return the ``FSFileName`` or throw an error.
        func setVolumeName(_ name: FSFileName) async throws -> FSFileName
    }

    /// Methods and properties implemented by volumes that support renaming the volume.
    ///
    /// > Important: This protocol replaces the ``FSVolumeRenameOperations`` protocol. It exposes the same functionality, while using the ``FSVolumeRenameResult`` object, to align with all other `Handler` protocols.
    @available(macOS 27.0, *)
    public protocol RenameHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isVolumeRenameInhibited: Bool { get }

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSVolumeRenameResult`` containing the ``FSFileName`` of the new volume name, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSVolumeRenameResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setVolumeName(_ name: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSVolumeRenameResult?, (any Error)?) -> Void)

        /// Sets a new name for the volume.
        ///
        /// - Parameters:
        ///   - name: The new volume name.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If renaming succeeds, pass an instance of ``FSVolumeRenameResult`` containing the ``FSFileName`` of the new volume name, along with a `nil` error. If renaming fails, pass the relevant error as the second parameter; FSKit ignores the ``FSVolumeRenameResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func setVolumeName(_ name: FSFileName, context: FSContext) async throws -> FSVolumeRenameResult
    }

    /// Behavior flags for preallocation operations.
    @available(macOS 15.4, *)
    public struct PreallocateFlags : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// Allocates contiguous space.
        public static var contiguous: FSVolume.PreallocateFlags { get }

        /// Allocates all requested space or no space at all.
        public static var all: FSVolume.PreallocateFlags { get }

        /// Allocates space that isn't freed when deleting the descriptor.
        ///
        /// This space remains allocated even after calling `close(2)`.
        public static var persist: FSVolume.PreallocateFlags { get }

        /// Allocates space from the physical end of file.
        ///
        /// When implementing this behavior, ignore any offset in the preallocate call.
        /// This flag is currently set for all ``FSVolume/PreallocateHandler/preallocateSpace(for:at:length:flags:context:replyHandler:)`` calls.
        public static var fromEOF: FSVolume.PreallocateFlags { get }
    }

    /// Methods and properties implemented by volumes that want to offer preallocation functions.
    ///
    /// A preallocation operation allocates space for a file without writing to it yet.
    /// A file system may use reallocation to avoid performing space allocation while in the midst of I/O; this strategy improves performance.
    /// Also, if the expected I/O pattern is many small writes, preallocating contiguous chunks may prevent fragmenting the file system.
    /// This process can improve performance later.
    ///
    /// In a kernel-based file system, you typically preallocate space with the `VNOP_ALLOCATE` operation, called from `fcntl(F_PREALLOCATE)`.
    ///
    /// > Deprecated: Use ``FSVolume/PreallocateHandler`` instead.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol PreallocateOperations : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isPreallocateInhibited: Bool { get set }

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there's no reply handler; simply return the allocated byte count or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there's no reply handler; simply return the allocated byte count or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags) async throws -> Int
    }

    /// Methods and properties implemented by volumes that want to offer preallocation functions.
    ///
    /// A preallocation operation allocates space for a file without writing to it yet.
    /// A file system may use reallocation to avoid performing space allocation while in the midst of I/O; this strategy improves performance.
    /// Also, if the expected I/O pattern is many small writes, preallocating contiguous chunks may prevent fragmenting the file system.
    /// This process can improve performance later.
    ///
    /// In a kernel-based file system, you typically preallocate space with the `VNOP_ALLOCATE` operation, called from `fcntl(F_PREALLOCATE)`.
    ///
    /// > Important: This protocol replaces the ``FSVolumePreallocateOperations`` protocol. It exposes the same functionality, while using the ``FSPreallocateResult`` object. This objects adds the ability to reply with ``FSItemAttributes`` and free space from ``preallocateSpace(for:at:length:flags:context:replyHandler:)``.
    @available(macOS 27.0, *)
    public protocol PreallocateHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isPreallocateInhibited: Bool { get }

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file and the volume's updated free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void)

        /// Preallocates disk space for the given item.
        ///
        /// - Parameters:
        ///   - item: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file and the volume's updated free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func preallocateSpace(for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext) async throws -> FSPreallocateResult
    }

    /// Options to specify the item deactivation policy.
    ///
    /// Callers may want to set a deactivation policy because ``FSVolume/ItemDeactivation/deactivateItem(_:)`` processing blocks the kernel.
    /// Setting a deactivation policy allows the file system to take action at a definitive point in the item's life cycle.
    /// These options allow the file system to instruct the FSKit kernel of which circumstances require the expense of a round-trip call to the module.
    ///
    /// > Note: To avoid performing deactivation calls, Objective-C developers use the value `FSItemDeactivationNever`. In Swift, use an empty option set (`[]`).
    @available(macOS 15.4, *)
    public struct ItemDeactivationOptions : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// An option to always perform deactivation calls.
        ///
        /// Use this option if the file system needs `deactivateItem` calls in circumstances beyond those covered by ``forRemovedItems`` and ``forPreallocatedItems``.
        public static var always: FSVolume.ItemDeactivationOptions { get }

        /// An option to process deactivation for open-unlinked items at the moment of last close.
        public static var forRemovedItems: FSVolume.ItemDeactivationOptions { get }

        /// An option to process deactivation for for files with preallocated space.
        ///
        /// This option facilitates a sort of trim-on-close behavior.
        /// It is only meaningful for volumes that conform to ``FSVolume/PreallocateHandler``.
        public static var forPreallocatedItems: FSVolume.ItemDeactivationOptions { get }
    }

    /// Methods and properties implemented by volumes that support deactivating items.
    @available(macOS, introduced: 15.4, deprecated: 27.0)
    public protocol ItemDeactivation : NSObjectProtocol {

        /// A property that tells FSKit to which types of items the deactivation applies, if any.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Operations/reclaimItem(_:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - reply: A block or closure to indicate success or failure. If deactivation fails, pass an error as the one parameter to the reply handler. If deactivation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Operations/reclaimItem(_:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - reply: A block or closure to indicate success or failure. If deactivation fails, pass an error as the one parameter to the reply handler. If deactivation succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
        func deactivateItem(_ item: FSItem) async throws
    }

    /// Methods and properties implemented by volumes that support deactivating items.
    ///
    /// > Important: This protocol replaces the ``FSVolumeItemDeactivation`` protocol. It exposes the same functionality, while using the ``FSDeactivateItemResult`` object. This object adds the ability to reply with free space from ``deactivateItem(_:context:replyHandler:)``.
    @available(macOS 27.0, *)
    public protocol ItemDeactivationHandler : NSObjectProtocol {

        /// A property that tells FSKit to which types of items the deactivation applies, if any.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        var itemDeactivationPolicy: FSVolume.ItemDeactivationOptions { get }

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Handler/reclaimItem(_:replyHandler:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If deactivation succeeds, pass an instance of ``FSDeactivateItemResult`` containing the volume's updated free space, along with a `nil` error. If deactivation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSDeactivateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func deactivateItem(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSDeactivateItemResult?, (any Error)?) -> Void)

        /// Notifies the file system that the kernel is no longer making immediate use of the given item.
        ///
        /// This method gives a file system a chance to release resources associated with an item.
        /// However, this method prescribes no specific action; it's acceptable to defer all reclamation until ``FSVolume/Handler/reclaimItem(_:replyHandler:)``.
        /// This method is the equivalent of VFS's `VNOP_INACTIVE`.
        ///
        /// FSKit restricts calls to this method based on the current value of ``FSVolume/ItemDeactivation/itemDeactivationPolicy``.
        ///
        /// - Parameters:
        ///   - item: The item to deactivate.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If deactivation succeeds, pass an instance of ``FSDeactivateItemResult`` containing the volume's updated free space, along with a `nil` error. If deactivation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSDeactivateItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func deactivateItem(_ item: FSItem, context: FSContext) async throws -> FSDeactivateItemResult
    }

    /// Types of region for seek operations
    @available(macOS 27.0, *)
    public enum SeekRegion : UInt, @unchecked Sendable {

        /// Seek the next hole region.
        ///
        /// When there are no more hole regions past the supplied `offset`,
        /// the current file size (end-of-file offset) should be returned.
        case hole = 1

        /// Seek the next data region.
        ///
        /// When there are no more data regions past the supplied `offset`,
        /// an error code `ENXIO` should be returned.
        case data = 2
    }

    /// Methods and properties implemented by volumes that support seek operations
    @available(macOS 27.0, *)
    public protocol SeekRegionHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        optional var isSeekRegionInhibited: Bool { get }

        /// Find the next offset of hole or data region greater than or equal to the
        /// supplied offset
        ///
        /// - Parameters:
        ///   - item: The item for which to seek.
        ///   - offset: The offset from which to seek.
        ///   - region: The region to seek.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If seek succeeds, pass an instance of ``FSSeekRegionResult`` containing the offset of the requested region greater than or equal to the supplied offset, along with a `nil` error. If seek fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSeekRegionResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext, replyHandler reply: @escaping @Sendable (FSSeekRegionResult?, (any Error)?) -> Void)

        /// Find the next offset of hole or data region greater than or equal to the
        /// supplied offset
        ///
        /// - Parameters:
        ///   - item: The item for which to seek.
        ///   - offset: The offset from which to seek.
        ///   - region: The region to seek.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If seek succeeds, pass an instance of ``FSSeekRegionResult`` containing the offset of the requested region greater than or equal to the supplied offset, along with a `nil` error. If seek fails, pass the relevant error as the second parameter; FSKit ignores the ``FSSeekRegionResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func seek(within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext) async throws -> FSSeekRegionResult
    }

    /// A type that identifies a volume.
    ///
    /// For most volumes, the volume identifier is the UUID identifying the volume.
    ///
    /// Network file systems may access the same underlying volume using different authentication credentials.
    /// To handle this situation, add qualifying data to identify the specific container, as discussed in the superclass, ``FSEntityIdentifier``.
    ///
    /// > Important: Don't subclass this class.
    @available(macOS 15.4, *)
    open class Identifier : FSEntityIdentifier {
    }

    /// Mount options to be requested from FSKit using the `requestedMountOptions` property.
    @available(macOS 26.4, *)
    public struct MountOptions : OptionSet, @unchecked Sendable {

        public init(rawValue: UInt)

        /// An option to request a read-only mount.
        public static var readOnly: FSVolume.MountOptions { get }
    }

    /// An enumeration of case-sensitivity support types.
    ///
    /// A case-sensitive volume is a volume that treats upper and lower case characters in file and directory names as being distinct from each other. For example, `FILE.TXT` and `file.TXT` are different names in a case-sensitive volume, and the same name in a case-insensitive volume.
    @available(macOS 15.4, *)
    public enum CaseFormat : Int, @unchecked Sendable {

        /// The volume is case sensitive.
        case sensitive = 0

        /// The volume isn't case sensitive.
        case insensitive = 1

        /// The volume isn't case sensitive, but supports preserving the case of file and directory names.
        case insensitiveCasePreserving = 2
    }

    /// A type that represents capabilities supported by a volume, such as hard and symbolic links, journaling, and large file sizes.
    @available(macOS 15.4, *)
    open class SupportedCapabilities : NSObject, NSSecureCoding {

        /// A Boolean property that indicates whether the volume supports persistent object identifiers and can look up file system objects by their IDs.
        open var supportsPersistentObjectIDs: Bool

        /// A Boolean property that indicates whether the volume supports symbolic links.
        open var supportsSymbolicLinks: Bool

        /// A Boolean property that indicates whether the volume supports hard links.
        open var supportsHardLinks: Bool

        /// A Boolean property that indicates whether the volume supports a journal used to speed recovery in case of unplanned restart, such as a power outage or crash.
        ///
        /// This property doesn't necessarily mean the volume is actively using a journal.
        open var supportsJournal: Bool

        /// A Boolean property that indicates whether the volume currently uses a journal for speeding recovery after an unplanned shutdown.
        open var supportsActiveJournal: Bool

        /// A Boolean property that indicates the volume doesn't store reliable times for the root directory.
        ///
        /// If this value is `true` (Swift) or `YES` (Objective-C), the volume doesn't store reliable times for the root directory.
        open var doesNotSupportRootTimes: Bool

        /// A Boolean property that indicates whether the volume supports sparse files.
        ///
        /// A sparse file is a file that can have "holes" that the file system has never written to, and as a result don't consume space on disk.
        open var supportsSparseFiles: Bool

        /// A Boolean property that indicates whether the volume supports zero runs
        ///
        /// If this value is true, the volume keeps track of allocated but unwritten runs of a file so that it can substitute zeroes without actually writing zeroes to the media.
        open var supportsZeroRuns: Bool

        /// A Boolean property that indicates whether the volume supports fast results when fetching file system statistics.
        ///
        /// A true value means this volume hints to upper layers to indicate that `statfs(2)` is fast enough that its results need not be cached by the caller.
        open var supportsFastStatFS: Bool

        /// A Boolean property that indicates whether the volume supports file sizes larger than 4GB, and potentially up to 2TB.
        open var supports2TBFiles: Bool

        /// A Boolean property that indicates whether the volume supports open deny modes.
        ///
        /// These are modes such as "open for read write, deny write".
        open var supportsOpenDenyModes: Bool

        /// A Boolean property that indicates whether the volume supports hidden files.
        ///
        /// A `true` value means the volume supports the `UF_HIDDEN` file flag.
        open var supportsHiddenFiles: Bool

        /// A Boolean property that indicates the volume doesn't support certain volume size reports.
        ///
        /// A true value means the volume doesn't support determining values for total data blocks, available blocks, or free blocks, as in `f_blocks`, `f_bavail`, and `f_bfree` in the struct `statFS` returned by `statfs(2)`.
        open var doesNotSupportVolumeSizes: Bool

        /// A Boolean property that indicates whether the volume supports 64-bit object IDs.
        open var supports64BitObjectIDs: Bool

        /// A Boolean property that indicates whether the volume supports document IDs for document revisions.
        ///
        /// A document ID is an identifier that persists across object ID changes.
        open var supportsDocumentID: Bool

        /// A Boolean property that indicates the volume doesn't support immutable files.
        ///
        /// A `true` value means this volume doesn't support setting the `UF_IMMUTABLE` flag.
        open var doesNotSupportImmutableFiles: Bool

        /// A Boolean property that indicates the volume doesn't set file permissions.
        ///
        /// If this value is `true` (Swift) or `YES` (Objective-C), the volume doesn't support setting file permissions.
        open var doesNotSupportSettingFilePermissions: Bool

        /// A Boolean property that indicates whether the volume supports multiple logical file systems that share space in a single "partition."
        open var supportsSharedSpace: Bool

        /// A Boolean property that indicates whether the volume supports volume groups.
        ///
        /// Volume groups involve multiple logical file systems that the system can mount and unmount together, and for which the system can present common file system identifier information.
        open var supportsVolumeGroups: Bool

        /// A value that indicates the volume's support for case sensitivity.
        open var caseFormat: FSVolume.CaseFormat
    }
}

/// A type used to report a volume's statistics.
///
/// The names of this type's properties match those in the `statfs` structure in `statfs(2)`, which reports these values for an FSKit file system.
/// All numeric properties default to `0`.
/// Override these values, unless a given property has no meaningful value to provide.
///
/// > Note: Available space, free space, total space, and used space have properties to express their values either as a number of blocks or a number of bytes.
/// Your module may supply both of these values by setting both the relevant block or byte property.
/// Alternatively, a module may set only one of the two properties.
/// When you do this, FSKit calculates the matching value based on ``blockSize``.
///
/// For the read-only ``fileSystemTypeName``, set this value with the designated initializer.
@available(macOS 15.4, *)
open class FSStatFSResult : NSObject, NSSecureCoding {

    /// A property for the volume's block size, in bytes.
    ///
    /// This value defaults to `4096`. Zero isn't a valid block size.
    open var blockSize: Int

    /// A property for the optimal block size with which to perform I/O.
    ///
    /// For best performance, specify an `ioSize` that's an even multiple of ``blockSize``.
    open var ioSize: Int

    /// A property for the volume's total data block count.
    open var totalBlocks: UInt64

    /// A property for the number of free blocks available to a non-superuser on the volume.
    open var availableBlocks: UInt64

    /// A property for the number of free blocks in the volume.
    open var freeBlocks: UInt64

    /// A property for the number of used blocks in the volume.
    open var usedBlocks: UInt64

    /// A property for the total size, in bytes, of the volume.
    open var totalBytes: UInt64

    /// A property for the amount of space available to users, in bytes, in the volume.
    open var availableBytes: UInt64

    /// A property for the amount of free space, in bytes, in the volume.
    open var freeBytes: UInt64

    /// A property for the amount of used space, in bytes, in the volume.
    open var usedBytes: UInt64

    /// A property for the total number of file slots in the volume,
    open var totalFiles: UInt64

    /// A property for the total number of free file slots in the volume.
    open var freeFiles: UInt64

    /// A property for the file system's subtype or flavor.
    ///
    /// Match this value to the `FSPersonalities`'s `FSSubType` attribute, if it exists within the `EXAppExtensionAttributes` dictionary of the module's `Info.plist`.
    open var fileSystemSubType: Int

    /// A property for the file system type name.
    ///
    /// Match this value to the `FSShortName` attribute within the `EXAppExtensionAttributes` dictionary of the module's `Info.plist`.
    /// The maximum allowed length is `MFSTYPENAMELEN`, including the terminating `NUL` character.
    open var fileSystemTypeName: String { get }

    /// Creates an statistics result instance, using the given file system type name.
    ///
    /// - Parameters fileSystemTypeName: A type name for the file system. The maximum allowed length is `MFSTYPENAMELEN`, including the terminating `NUL` character.
    public init(fileSystemTypeName: String)
}

@available(macOS 27.0, *)
extension FSVolume {

    /// Sends a synchronous cache state update request from the module to the kernel.
    ///
    /// Volumes conforming to ``FSVolumeDataCacheHandler`` call this method to proactively notify
    /// the kernel about cache policy changes that need to be applied immediately.
    /// This allows module-initiated updates outside the normal open/close/upgrade/downgrade flow.
    ///
    /// When downgrading coherency type, the action must be ``FSVolume/KernelCacheCoherencyAction/push``,
    /// ``FSVolume/KernelCacheCoherencyAction/pushInvalidate``, or ``FSVolume/KernelCacheCoherencyAction/invalidate``
    /// to instruct the kernel how to handle cached data.
    /// If the action fails, the cache state remains unchanged and the method returns an error.
    ///
    /// > Important: This method must be called without holding any module-internal locks.
    /// The kernel may issue additional operations back into the module to satisfy cache state changes,
    /// which could result in deadlock if locks are held.
    ///
    /// > Note: This method is only functional for volumes that conform to ``FSVolumeDataCacheHandler``.
    /// For volumes that don't conform to the protocol, this method returns `ENOTSUP`.
    ///
    /// - Parameters:
    ///   - item: The item for which to update the cache state.
    ///   - cacheMode: The new cache mode to apply.
    ///   - coherencyType: The new coherency type to apply.
    ///   - action: The action for the kernel to perform on cached data (push, invalidate, update, or revoke).
    /// - Returns: An error if the kernel was unable to complete the requested cache state change, or `nil` on success.
    @available(macOS 27.0, *)
    open func setCacheState(for item: FSItem, cacheMode: FSVolume.DataCacheMode, coherencyType: FSVolume.KernelCacheCoherencyType, action: FSVolume.KernelCacheCoherencyAction) -> (any Error)?
}

extension FSVolume {

    /// A type that defines the cache mode requested by the kernel for data operations.
    @available(macOS 27.0, *)
    public enum DataCacheMode : Int, @unchecked Sendable {

        /// A mode that indicates no active caching.
        case none = 0

        /// A mode that indicates read access with caching enabled.
        case readWithCache = 1

        /// A mode that indicates read-write access with caching enabled.
        case readWriteWithCache = 2
    }

    /// A type that defines how the kernel caches data.
    @available(macOS 27.0, *)
    public enum KernelCacheCoherencyType : Int, @unchecked Sendable {

        /// A type that indicates all I/O goes directly to storage, without caching.
        case noCache = 0

        /// A type that indicates that writes bypass the cache and go directly to storage.
        case readCache = 1

        /// A type that indicates writes update cache and storage synchronously.
        case writeThrough = 2

        /// A type that indicates writes immediately update the cache only, followed by a deferred write to storage.
        case writeBack = 3
    }

    /// A type that defines actions for cache state changes.
    @available(macOS 27.0, *)
    public enum KernelCacheCoherencyAction : Int, @unchecked Sendable {

        /// An action to flush dirty data from cache to storage, preserving cache contents.
        case push = 0

        /// An action to flush dirty data to storage and invalidate (clear) the cache.
        /// This also invalidates the item's cached attributes, so the next request
        /// for attributes fetches them from your module.
        ///
        /// For a directory, this also purges the kernel name cache for the
        /// directory's entries. With the name cache purged, any later lookup re-populates the cache.
        case pushInvalidate = 1

        /// An action to invalidate (clear) the cache, discarding any dirty data without writing to storage.
        /// This also invalidates the item's cached attributes, so the next request
        /// for attributes fetches them from your module.
        ///
        /// For a directory, this also purges the kernel name cache for the
        /// directory's entries. With the name cache purged, any later lookup re-populates the cache.
        case invalidate = 2

        /// An action to update the coherency mode while keeping the cache valid, requiring no push or invalidation.
        case update = 3

        /// An action to invalidate all caches, revoke all access to the item, and trigger vnode reclamation.
        ///
        /// Use this action when the module determines that an item no longer exists or
        /// is no longer accessible. Common scenarios include:
        /// - Another client deleted the item, as detected via server notification.
        /// - The module received a server callback indicating the file's absence.
        case revoke = 4
    }

    /// Methods and properties implemented by volumes that coordinate kernel-level data caching.
    ///
    /// A volume that conforms to this protocol enables kernel data caching for improved I/O performance.
    /// This protocol allows filesystem modules to negotiate cache modes with the kernel and manage cache coherency.
    ///
    /// > Note: This protocol handles the complete open and close lifecycle for items. When a volume conforms
    /// to `FSVolumeDataCacheHandler`, FSKit routes every open and close operation through this protocol's
    /// ``open(_:modes:cacheMode:context:replyHandler:)`` and ``close(_:context:replyHandler:)`` methods.
    /// A volume that adopts this protocol doesn't need to also conform to ``FSVolumeOpenCloseHandler``.
    ///
    /// When a file opens, the module receives the requested ``FSVolume/DataCacheMode`` and returns a
    /// ``FSVolume/KernelCacheCoherencyType`` indicating the kind of caching behavior it can support. The kernel then
    /// caches data according to the granted coherency type. The module can dynamically upgrade or
    /// downgrade cache modes as conditions change.
    ///
    /// The kernel requests a caching mode expressed as a ``FSVolume/DataCacheMode`` value, which indicates
    /// what the kernel would like to cache (read-only data, read-write data, or no caching). The module
    /// then replies with a specific ``FSVolume/KernelCacheCoherencyType`` value, which defines how the kernel
    /// should cache the data (no caching, read-only caching, write-through caching, or write-back caching).
    /// When the module detects an asynchronous condition requiring a change in caching mode (such as an
    /// lease break), the module uses a value from ``FSVolume/KernelCacheCoherencyAction`` to instruct the kernel
    /// how to handle any cached data (push dirty pages, invalidate cache, or update coherency mode).
    ///
    /// The protocol supports deferred closing, where the kernel maintains cache state even after a file
    /// is closed, enabling improved performance for frequently accessed files. The ``FSVolume/KernelCacheCoherencyType/readCache``,
    /// ``FSVolume/KernelCacheCoherencyType/writeThrough``, and ``FSVolume/KernelCacheCoherencyType/writeBack`` modes
    /// support deferred closing.
    ///
    /// The following table shows the mapping of cache modes to supported coherency types.
    ///
    /// | Cache mode | Coherency type |
    /// |------------|-----------------|
    /// | ``FSVolume/DataCacheMode/none`` | ``FSVolume/KernelCacheCoherencyType/noCache`` |
    /// | ``FSVolume/DataCacheMode/readWithCache`` | ``FSVolume/KernelCacheCoherencyType/noCache`` or ``FSVolume/KernelCacheCoherencyType/readCache`` |
    /// | ``FSVolume/DataCacheMode/readWriteWithCache`` | ``FSVolume/KernelCacheCoherencyType/noCache``, ``FSVolume/KernelCacheCoherencyType/readCache``, ``FSVolume/KernelCacheCoherencyType/writeBack`` or ``FSVolume/KernelCacheCoherencyType/writeThrough`` |
    ///
    /// ### Supporting coherency transitions
    ///
    /// Transitioning between coherency types requires different behaviors from your volume implementation, depending on whether the new type is more or less permissive than its current value.
    /// The following table expresses the permissiveness of the coherency types.
    ///
    /// | Coherency type | Permissiveness |
    /// |----------------|----------------|
    /// | ``FSVolume/KernelCacheCoherencyType/noCache`` | Least permissive |
    /// | ``FSVolume/KernelCacheCoherencyType/readCache`` |  |
    /// | ``FSVolume/KernelCacheCoherencyType/writeBack`` |  |
    /// | ``FSVolume/KernelCacheCoherencyType/writeThrough`` | Most permissive |
    ///
    /// When transitioning to more permissive caching, kernel performs an "upgrade" by calling ``upgrade(_:cacheMode:context:replyHandler:)``.
    /// Your volume doesn't need to perform a flush or purge when upgrading to a more permissive coherency type.
    ///
    /// Transitioning to a less permissive coherency type is considered a "downgrade".
    /// Your module initiates this process by calling ``FSVolume/setCacheState(for:cacheMode:coherencyType:action:)`` when conditions change.
    /// In this scenario, set the `action` to ``FSVolume/KernelCacheCoherencyAction/push``, ``FSVolume/KernelCacheCoherencyAction/pushInvalidate``, or ``FSVolume/KernelCacheCoherencyAction/invalidate``.
    /// Handle any dirty data by flushing or purging it before downgrading with this method call.
    ///
    /// > Important: If a file system doesn't conform to this protocol, the kernel may still cache it.
    /// However, such a file system has no control over caching behavior; the kernel caches data as it sees fit.
    @available(macOS 27.0, *)
    public protocol DataCacheHandler : NSObjectProtocol {

        /// A Boolean value that instructs FSKit not to call this protocol's methods, even if the volume conforms to it.
        ///
        /// FSKit reads this value after the file system replies to the `loadResource` message.
        /// Changing the returned value during the runtime of the volume has no effect.
        ///
        /// > Note: This property does not disable data caching; it only disables the usage of the protocol. In order to completely disable data caching, return the ``FSVolume/KernelCacheCoherencyType/noCache`` coherency type from ``open(_:modes:cacheMode:context:replyHandler:)`` for all items.
        @available(macOS 27.0, *)
        optional var isDataCacheInhibited: Bool { get }

        /// Opens an item with cache mode negotiation.
        ///
        /// FSKit calls this method when opening a file, providing the requested cache mode.
        /// The module implementation determines what level of caching it can support for this item,
        /// considering factors such as server lease availability, file locking state, or other coherency requirements.
        ///
        /// The granted coherency type must be compatible with the requested cache mode, as defined
        /// by the cache-mode-to-coherency-type mappings documented in the discussion of the ``FSVolume/DataCacheHandler`` protocol. If the module grants
        /// a coherency type that exceeds the cache mode's permissions, the kernel downgrades to a valid coherency type.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The open modes, such as read and write.
        ///   - cacheMode: The requested cache mode, indicating what data is eligible for caching.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening succeeds, pass an instance of ``FSOpenItemResult`` containing the granted ``FSVolume/KernelCacheCoherencyType``, along with a `nil` error. If opening fails, pass the relevant error as the second parameter; FSKit ignores the ``FSOpenItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        @available(macOS 27.0, *)
        func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSOpenItemResult?, (any Error)?) -> Void)

        /// Opens an item with cache mode negotiation.
        ///
        /// FSKit calls this method when opening a file, providing the requested cache mode.
        /// The module implementation determines what level of caching it can support for this item,
        /// considering factors such as server lease availability, file locking state, or other coherency requirements.
        ///
        /// The granted coherency type must be compatible with the requested cache mode, as defined
        /// by the cache-mode-to-coherency-type mappings documented in the discussion of the ``FSVolume/DataCacheHandler`` protocol. If the module grants
        /// a coherency type that exceeds the cache mode's permissions, the kernel downgrades to a valid coherency type.
        ///
        /// - Parameters:
        ///   - item: The item to open.
        ///   - modes: The open modes, such as read and write.
        ///   - cacheMode: The requested cache mode, indicating what data is eligible for caching.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If opening succeeds, pass an instance of ``FSOpenItemResult`` containing the granted ``FSVolume/KernelCacheCoherencyType``, along with a `nil` error. If opening fails, pass the relevant error as the second parameter; FSKit ignores the ``FSOpenItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        @available(macOS 27.0, *)
        func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext) async throws -> FSOpenItemResult

        /// Closes an item and releases associated cache resources.
        ///
        /// FSKit calls this method when fully closing a file, and after the kernel finalizes all caching for the item.
        ///
        /// Your module receives this call once per item when all references are released and the kernel
        /// has completed its cache management. The module performs any necessary cleanup
        /// operations for the item.
        ///
        /// > Note: This method doesn't return or throw an error because the OS considers the file closed regardless
        /// of whether the module encounters any issues during cleanup.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to call when the close operation completes.
        @available(macOS 27.0, *)
        func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void)

        /// Closes an item and releases associated cache resources.
        ///
        /// FSKit calls this method when fully closing a file, and after the kernel finalizes all caching for the item.
        ///
        /// Your module receives this call once per item when all references are released and the kernel
        /// has completed its cache management. The module performs any necessary cleanup
        /// operations for the item.
        ///
        /// > Note: This method doesn't return or throw an error because the OS considers the file closed regardless
        /// of whether the module encounters any issues during cleanup.
        ///
        /// - Parameters:
        ///   - item: The item to close.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to call when the close operation completes.
        @available(macOS 27.0, *)
        func close(_ item: FSItem, context: FSContext) async

        /// Upgrades the item cache mode to a less restrictive level, allowing more caching.
        ///
        /// FSKit calls this method when transitioning to a cache mode that allows more aggressive caching.
        ///
        /// - Parameters:
        ///   - item: The item for which to upgrade the cache mode.
        ///   - cacheMode: The new (more permissive) cache mode being requested.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If successful, pass an instance of ``FSUpgradeItemResult`` containing the granted ``FSVolume/KernelCacheCoherencyType``, along with a `nil` error. If upgrading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSUpgradeItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        @available(macOS 27.0, *)
        func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSUpgradeItemResult?, (any Error)?) -> Void)

        /// Upgrades the item cache mode to a less restrictive level, allowing more caching.
        ///
        /// FSKit calls this method when transitioning to a cache mode that allows more aggressive caching.
        ///
        /// - Parameters:
        ///   - item: The item for which to upgrade the cache mode.
        ///   - cacheMode: The new (more permissive) cache mode being requested.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If successful, pass an instance of ``FSUpgradeItemResult`` containing the granted ``FSVolume/KernelCacheCoherencyType``, along with a `nil` error. If upgrading fails, pass the relevant error as the second parameter; FSKit ignores the ``FSUpgradeItemResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        @available(macOS 27.0, *)
        func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext) async throws -> FSUpgradeItemResult
    }
}

/// A unique identifier for an operation.
@available(macOS 15.4, *)
public struct FSOperationID : Hashable, Equatable, RawRepresentable, @unchecked Sendable {

    public init(_ rawValue: Int)

    public init(rawValue: Int)
}

extension FSOperationID {

    @available(macOS 15.4, *)
    public static let unspecified: FSOperationID
}

/// Flags that describe the behavior of a blockmap operation.
///
/// This type is an option set in Swift.
/// In Objective-C, you use the cases of this enumeration to create a bit field.
@available(macOS 15.4, *)
public struct FSBlockmapFlags : OptionSet, @unchecked Sendable {

    public init(rawValue: UInt)

    /// A flag that describes a read operation.
    public static var read: FSBlockmapFlags { get }

    /// A flag that describes a write operation.
    public static var write: FSBlockmapFlags { get }
}

/// Flags that describe the behavior of an I/O completion operation.
///
/// This type is an option set in Swift.
/// In Objective-C, the cases of this enumeration combine to create a bit field.
@available(macOS 15.4, *)
public struct FSCompleteIOFlags : OptionSet, @unchecked Sendable {

    public init(rawValue: UInt)

    /// A flag that describes a read operation.
    public static var read: FSCompleteIOFlags { get }

    /// A flag that describes a write operation.
    public static var write: FSCompleteIOFlags { get }

    /// A flag that requests that the file system module flush metadata I/O asynchronously.
    public static var async: FSCompleteIOFlags { get }
}

/// An enumeration of types of extents.
@available(macOS 15.4, *)
public enum FSExtentType : Int, @unchecked Sendable {

    /// An extent type to indicate valid data.
    ///
    /// Use this type for all extents on a file system that doesn't support sparse files.
    ///
    /// > Tip: The kernel keeps track of the end of file, so it knows a range of `[EOF, allocated space]` is uninitialized. Because of this behavior, it's valid to pass the data extent type for such a range.
    case data = 0

    /// An extent type to indicate uninitialized data.
    ///
    /// Only use this extent type in file systems that support sparse files, and only then to represent ranges in the file that aren't allocated yet.
    case zeroFill = 1

    /// An extent type to indicate read-only data.
    ///
    /// Use this type to represent ranges in the file that data can be read but not written to.
    @available(macOS 27.0, *)
    case readOnly = 2
}

/// A type that directs the kernel to map space on disk to a specific file managed by this file system.
///
/// _Extents_ provide the kernel the logical-to-physical mapping of a given file.
/// An extent describes a physical offset on disk, and a length and a logical offset within the file.
/// Rather than working with extents directly, you use this type's methods to provide or "pack" extent information, which FSKit then passes to the kernel.
@available(macOS 15.4, *)
open class FSExtentPacker : NSObject {

    /// Packs a single extent to send to the kernel.
    ///
    /// - Parameters:
    ///   - resource: The resource on which to perform I/O.
    ///   - type: The type of extent, indicating whether it contains valid data.
    ///   - logicalOffset: The extent offset within the file, in bytes.
    ///   - physicalOffset: The extent offset on disk, in bytes.
    ///   - length: The extent length, in bytes. The maximal valid length is `UINT32_MAX`.
    /// - Returns: A Boolean value that indicates whether the packer can pack more extents.
    open func packExtent(resource: FSBlockDeviceResource, type: FSExtentType, logicalOffset: off_t, physicalOffset: off_t, length: Int) -> Bool
}

/// Methods and properties implemented by volumes that use kernel-offloaded I/O to achieve higher file transfer performance.
///
/// A volume that conforms to this protocol supplies file extent mappings to FSKit, which allows file data transfers to take place in the kernel.
/// This approach provides higher-performance data transfer than transferring all file data between the module and kernel, while still allowing the file system to run in user space.
///
/// This protocol uses _extents_ to provide the kernel the logical-to-physical mapping of a given file.
/// An extent describes a physical offset on disk, and a length and a logical offset within the file.
/// You don't manage extents directly.
/// Instead, FSKit provides you with an ``FSExtentPacker`` to define and pack the extents in your implementations of this protocol's methods.
///
/// Most volumes conform to either this protocol or ``FSVolumeReadWriteOperations``.
/// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
/// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use ``FSVolumeReadWriteOperations``, and those without it use this protocol.
/// A volume that doesn't conform to either protocol can't support any file I/O operation.
///
/// > Deprecated: Use ``FSVolume/KernelOffloadedIOHandler`` instead.
@available(macOS, introduced: 15.4, deprecated: 27.0)
public protocol FSVolumeKernelOffloadedIOOperations : NSObjectProtocol {

    /// Maps a file's disk space into extents, allowing the kernel to perform I/O with that space.
    ///
    /// FSKit calls this method when the kernel needs to get a mapping of logical-to-physical offsets of the file's data.
    /// This call may occur as part of an I/O operation on the file, or just to get the mapping as part of an `fcntl(F_LOG2PHYS)` system call.
    /// In the case of an I/O operation on the file, `operationID` has a nonzero value; a future call to ``completeIO(for:offset:length:status:flags:operationID:)`` uses the same `operationID` to indicate which operation it completes.
    /// In the case of an `fcntl(F_LOG2PHYS)` system call, the `operationID` parameter is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
    /// In both cases the kernel retains the mapping, and it may perform I/O to this range (or a part of it) at any time.
    ///
    /// If satisfying a blockmap request requires more extents than `packer` can handle, FSKit makes additional calls to this method with the same operation ID to collect the remainder.
    ///
    /// - Parameters:
    ///   - file: The file for which to map disk space.
    ///   - offset: The starting logical offset of the range to be mapped (in bytes).
    ///   - length: The length of the range to be mapped (in bytes).
    ///   - flags: Flags that affect the behavior of the blockmap operation.
    ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) indicates the beginning of an I/O operation. A value of `0` or ``FSOperationID/unspecified`` indicates the kernel maps the file without performing I/O. In this case, FSKit doesn't perform a corresponding call to ``completeIO(for:offset:length:status:flags:operationID:)``.
    ///   - packer: An extent packer you use to pack the requested range of the file's allocated disk space. FSKit sends all of the packed extents to the kernel when it invokes `reply`.
    ///   - reply: A block or closure to indicate success or failure. If mapping fails, pass an error as the one parameter to the reply handler. If mapping succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
    func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

    /// Maps a file's disk space into extents, allowing the kernel to perform I/O with that space.
    ///
    /// FSKit calls this method when the kernel needs to get a mapping of logical-to-physical offsets of the file's data.
    /// This call may occur as part of an I/O operation on the file, or just to get the mapping as part of an `fcntl(F_LOG2PHYS)` system call.
    /// In the case of an I/O operation on the file, `operationID` has a nonzero value; a future call to ``completeIO(for:offset:length:status:flags:operationID:)`` uses the same `operationID` to indicate which operation it completes.
    /// In the case of an `fcntl(F_LOG2PHYS)` system call, the `operationID` parameter is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
    /// In both cases the kernel retains the mapping, and it may perform I/O to this range (or a part of it) at any time.
    ///
    /// If satisfying a blockmap request requires more extents than `packer` can handle, FSKit makes additional calls to this method with the same operation ID to collect the remainder.
    ///
    /// - Parameters:
    ///   - file: The file for which to map disk space.
    ///   - offset: The starting logical offset of the range to be mapped (in bytes).
    ///   - length: The length of the range to be mapped (in bytes).
    ///   - flags: Flags that affect the behavior of the blockmap operation.
    ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) indicates the beginning of an I/O operation. A value of `0` or ``FSOperationID/unspecified`` indicates the kernel maps the file without performing I/O. In this case, FSKit doesn't perform a corresponding call to ``completeIO(for:offset:length:status:flags:operationID:)``.
    ///   - packer: An extent packer you use to pack the requested range of the file's allocated disk space. FSKit sends all of the packed extents to the kernel when it invokes `reply`.
    ///   - reply: A block or closure to indicate success or failure. If mapping fails, pass an error as the one parameter to the reply handler. If mapping succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
    func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker) async throws

    /// Completes an I/O operation for a given file.
    ///
    /// Implement this method by updating a file's metadata, such as its size and modification time.
    ///
    /// FSKit may call this method without an earlier call to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// In this case, the `operationID` is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
    ///
    /// - Parameters:
    ///   - file: The file for which the I/O operation completed.
    ///   - offset: The starting logical offset at which I/O started.
    ///   - length: The length of the I/O range (in bytes).
    ///   - status: Any error that occurred during the operation. If no error occurred, this parameter is `nil`.
    ///   - flags: Flags that affect the behavior of the complete I/O operation.
    ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) corresponds to a previous call to ``blockmapFile(_:offset:length:flags:operationID:packer:)`` with the same `operationID`.
    ///   - reply: A block or closure to indicate success or failure. If completing I/O fails, pass an error as the one parameter to the reply handler. If completing I/O succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
    func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void)

    /// Completes an I/O operation for a given file.
    ///
    /// Implement this method by updating a file's metadata, such as its size and modification time.
    ///
    /// FSKit may call this method without an earlier call to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// In this case, the `operationID` is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
    ///
    /// - Parameters:
    ///   - file: The file for which the I/O operation completed.
    ///   - offset: The starting logical offset at which I/O started.
    ///   - length: The length of the I/O range (in bytes).
    ///   - status: Any error that occurred during the operation. If no error occurred, this parameter is `nil`.
    ///   - flags: Flags that affect the behavior of the complete I/O operation.
    ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) corresponds to a previous call to ``blockmapFile(_:offset:length:flags:operationID:packer:)`` with the same `operationID`.
    ///   - reply: A block or closure to indicate success or failure. If completing I/O fails, pass an error as the one parameter to the reply handler. If completing I/O succeeds, pass `nil`. For an `async` Swift implementation, there's no reply handler; simply throw an error or return normally.
    func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID) async throws

    /// Creates a new file item and map its disk space.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// Only perform this technique opportunistically.
    /// In particular, don't perform additional I/O to fetch extent data.
    ///
    /// Packing extents in this method requires that `attributes` defines a size greater than 0.
    ///
    /// An implementation that doesn't supply the extents can ignore the packer and call the corresponding method in the ``FSVolume/Operations`` protocol, ``FSVolume/Operations/createItem(named:type:inDirectory:attributes:)``.
    ///
    /// - Parameters:
    ///   - name: The new file's name.
    ///   - directory: The directory in which to create the file.
    ///   - attributes: Attributes to apply to the new file.
    ///   - packer: An extent packer you use to pack the file's allocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; instead, return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
    func createFile(name: FSFileName, in directory: FSItem, attributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

    /// Creates a new file item and map its disk space.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// Only perform this technique opportunistically.
    /// In particular, don't perform additional I/O to fetch extent data.
    ///
    /// Packing extents in this method requires that `attributes` defines a size greater than 0.
    ///
    /// An implementation that doesn't supply the extents can ignore the packer and call the corresponding method in the ``FSVolume/Operations`` protocol, ``FSVolume/Operations/createItem(named:type:inDirectory:attributes:)``.
    ///
    /// - Parameters:
    ///   - name: The new file's name.
    ///   - directory: The directory in which to create the file.
    ///   - attributes: Attributes to apply to the new file.
    ///   - packer: An extent packer you use to pack the file's allocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass the newly created ``FSItem`` and its ``FSFileName``, along with a `nil` error. If creation fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; instead, return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
    func createFile(name: FSFileName, in directory: FSItem, attributes: FSItem.SetAttributesRequest, packer: FSExtentPacker) async throws -> (FSItem, FSFileName)

    /// Looks up an item within a directory and maps its disk space.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// Only perform this technique opportunistically.
    /// In particular, don't perform additional I/O to fetch extent data.
    ///
    /// - Parameters:
    ///   - name: The name of the file to look up.
    ///   - directory: The directory in which to look up the file.
    ///   - packer: An extent packer you use to pack the file's allocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName``, along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; instead, return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
    func lookupItem(name: FSFileName, in directory: FSItem, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSItem?, FSFileName?, (any Error)?) -> Void)

    /// Looks up an item within a directory and maps its disk space.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    /// Only perform this technique opportunistically.
    /// In particular, don't perform additional I/O to fetch extent data.
    ///
    /// - Parameters:
    ///   - name: The name of the file to look up.
    ///   - directory: The directory in which to look up the file.
    ///   - packer: An extent packer you use to pack the file's allocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass the found ``FSItem`` and its ``FSFileName``, along with a `nil` error. If lookup fails, pass the relevant error as the third parameter; FSKit ignores any ``FSItem`` or ``FSFileName`` in this case. For an `async` Swift implementation, there's no reply handler; instead, return a tuple of the ``FSItem`` and its ``FSFileName`` or throw an error.
    func lookupItem(name: FSFileName, in directory: FSItem, packer: FSExtentPacker) async throws -> (FSItem, FSFileName)

    /// Preallocates and maps disk space for the given file.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    ///
    /// > Important: Only implement this method if your file system conforms to ``FSVolume/PreallocateOperations``.
    ///
    /// - Parameters:
    ///   - file: The item for which to preallocate space.
    ///   - offset: The offset from which to allocate.
    ///   - length: The length of the space in bytes.
    ///   - flags: Flags that affect the preallocation behavior.
    ///   - packer: An extent packer you use to pack the file's preallocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a nil error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there’s no reply handler; simply return the allocated byte count or throw an error.
    optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (Int, (any Error)?) -> Void)

    /// Preallocates and maps disk space for the given file.
    ///
    /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:)``.
    ///
    /// > Important: Only implement this method if your file system conforms to ``FSVolume/PreallocateOperations``.
    ///
    /// - Parameters:
    ///   - file: The item for which to preallocate space.
    ///   - offset: The offset from which to allocate.
    ///   - length: The length of the space in bytes.
    ///   - flags: Flags that affect the preallocation behavior.
    ///   - packer: An extent packer you use to pack the file's preallocated disk space.
    ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass the amount of bytes allocated and a nil error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores any byte count in this case. For an `async` Swift implementation, there’s no reply handler; simply return the allocated byte count or throw an error.
    optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker) async throws -> Int
}

extension FSVolume {

    /// Methods and properties implemented by volumes that use kernel-offloaded I/O to achieve higher file transfer performance.
    ///
    /// A volume that conforms to this protocol supplies file extent mappings to FSKit, which allows file data transfers to take place in the kernel.
    /// This approach provides higher-performance data transfer than transferring all file data between the module and kernel, while still allowing the file system to run in user space.
    ///
    /// This protocol uses _extents_ to provide the kernel the logical-to-physical mapping of a given file.
    /// An extent describes a physical offset on disk, and a length and a logical offset within the file.
    /// You don't manage extents directly.
    /// Instead, FSKit provides you with an ``FSExtentPacker`` to define and pack the extents in your implementations of this protocol's methods.
    ///
    /// Most volumes conform to either this protocol or ``FSVolumeReadWriteHandler``.
    /// You can conform to both if you need to provide kernel-offloaded I/O only for certain files.
    /// In that case, files with the ``FSItem/Attribute/inhibitKernelOffloadedIO`` attribute set use ``FSVolumeReadWriteHandler``, and those without it use this protocol.
    /// A volume that doesn't conform to either protocol can't support any file I/O operation.
    ///
    /// > Important: This protocol replaces the ``FSVolumeKernelOffloadedIOOperations`` protocol. It exposes the same functionality, while using ``FSVolumeHandlerResult`` objects. These objects add the ability to reply with ``FSItemAttributes`` and free space from the relevant methods.
    @available(macOS 27.0, *)
    public protocol KernelOffloadedIOHandler : NSObjectProtocol {

        /// Maps a file's disk space into extents, allowing the kernel to perform I/O with that space.
        ///
        /// FSKit calls this method when the kernel needs to get a mapping of logical-to-physical offsets of the file's data.
        /// This call may occur as part of an I/O operation on the file, or just to get the mapping as part of an `fcntl(F_LOG2PHYS)` system call.
        /// In the case of an I/O operation on the file, `operationID` has a nonzero value; a future call to ``completeIO(for:offset:length:status:flags:operationID:replyHandler:)`` uses the same `operationID` to indicate which operation it completes.
        /// In the case of an `fcntl(F_LOG2PHYS)` system call, the `operationID` parameter is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
        /// In both cases the kernel retains the mapping, and it may perform I/O to this range (or a part of it) at any time.
        ///
        /// If satisfying a blockmap request requires more extents than `packer` can handle, FSKit makes additional calls to this method with the same operation ID to collect the remainder.
        ///
        /// - Parameters:
        ///   - file: The file for which to map disk space.
        ///   - offset: The starting logical offset of the range to be mapped (in bytes).
        ///   - length: The length of the range to be mapped (in bytes).
        ///   - flags: Flags that affect the behavior of the blockmap operation.
        ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) indicates the beginning of an I/O operation. A value of `0` or ``FSOperationID/unspecified`` indicates the kernel maps the file without performing I/O. In this case, FSKit doesn't perform a corresponding call to ``completeIO(for:offset:length:status:flags:operationID:replyHandler:)``.
        ///   - packer: An extent packer you use to pack the requested range of the file's allocated disk space. FSKit sends all of the packed extents to the kernel when it invokes `reply`.
        ///   - reply: A block or closure to indicate success or failure. If mapping succeeds, pass an instance of ``FSBlockmapResult`` containing the volume's updated free space, along with a `nil` error. If mapping fails, pass the relevant error as the second parameter; FSKit ignores the ``FSBlockmapResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSBlockmapResult?, (any Error)?) -> Void)

        /// Maps a file's disk space into extents, allowing the kernel to perform I/O with that space.
        ///
        /// FSKit calls this method when the kernel needs to get a mapping of logical-to-physical offsets of the file's data.
        /// This call may occur as part of an I/O operation on the file, or just to get the mapping as part of an `fcntl(F_LOG2PHYS)` system call.
        /// In the case of an I/O operation on the file, `operationID` has a nonzero value; a future call to ``completeIO(for:offset:length:status:flags:operationID:replyHandler:)`` uses the same `operationID` to indicate which operation it completes.
        /// In the case of an `fcntl(F_LOG2PHYS)` system call, the `operationID` parameter is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
        /// In both cases the kernel retains the mapping, and it may perform I/O to this range (or a part of it) at any time.
        ///
        /// If satisfying a blockmap request requires more extents than `packer` can handle, FSKit makes additional calls to this method with the same operation ID to collect the remainder.
        ///
        /// - Parameters:
        ///   - file: The file for which to map disk space.
        ///   - offset: The starting logical offset of the range to be mapped (in bytes).
        ///   - length: The length of the range to be mapped (in bytes).
        ///   - flags: Flags that affect the behavior of the blockmap operation.
        ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) indicates the beginning of an I/O operation. A value of `0` or ``FSOperationID/unspecified`` indicates the kernel maps the file without performing I/O. In this case, FSKit doesn't perform a corresponding call to ``completeIO(for:offset:length:status:flags:operationID:replyHandler:)``.
        ///   - packer: An extent packer you use to pack the requested range of the file's allocated disk space. FSKit sends all of the packed extents to the kernel when it invokes `reply`.
        ///   - reply: A block or closure to indicate success or failure. If mapping succeeds, pass an instance of ``FSBlockmapResult`` containing the volume's updated free space, along with a `nil` error. If mapping fails, pass the relevant error as the second parameter; FSKit ignores the ``FSBlockmapResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func blockmapFile(_ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID, packer: FSExtentPacker) async throws -> FSBlockmapResult

        /// Completes an I/O operation for a given file.
        ///
        /// Implement this method by updating a file's metadata, such as its size and modification time.
        ///
        /// FSKit may call this method without an earlier call to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// In this case, the `operationID` is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
        ///
        /// - Parameters:
        ///   - file: The file for which the I/O operation completed.
        ///   - offset: The starting logical offset at which I/O started.
        ///   - length: The length of the I/O range (in bytes).
        ///   - status: Any error that occurred during the operation. If no error occurred, this parameter is `nil`.
        ///   - flags: Flags that affect the behavior of the complete I/O operation.
        ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) corresponds to a previous call to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)`` with the same `operationID`.
        ///   - reply: A block or closure to indicate success or failure. If completing I/O succeeds, pass an instance of ``FSCompleteIOResult`` containing the updated ``FSItemAttributes`` of the file, along with a `nil` error. If completing I/O fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCompleteIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID, replyHandler reply: @escaping @Sendable (FSCompleteIOResult?, (any Error)?) -> Void)

        /// Completes an I/O operation for a given file.
        ///
        /// Implement this method by updating a file's metadata, such as its size and modification time.
        ///
        /// FSKit may call this method without an earlier call to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// In this case, the `operationID` is `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift).
        ///
        /// - Parameters:
        ///   - file: The file for which the I/O operation completed.
        ///   - offset: The starting logical offset at which I/O started.
        ///   - length: The length of the I/O range (in bytes).
        ///   - status: Any error that occurred during the operation. If no error occurred, this parameter is `nil`.
        ///   - flags: Flags that affect the behavior of the complete I/O operation.
        ///   - operationID: A unique identifier of the blockmap call. Any value other than `0` (Objective-C) or ``FSOperationID/unspecified`` (Swift) corresponds to a previous call to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)`` with the same `operationID`.
        ///   - reply: A block or closure to indicate success or failure. If completing I/O succeeds, pass an instance of ``FSCompleteIOResult`` containing the updated ``FSItemAttributes`` of the file, along with a `nil` error. If completing I/O fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCompleteIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func completeIO(for file: FSItem, offset: off_t, length: Int, status: any Error, flags: FSCompleteIOFlags, operationID: FSOperationID) async throws -> FSCompleteIOResult

        /// Creates a new file item and map its disk space.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// Only perform this technique opportunistically.
        /// In particular, don't perform additional I/O to fetch extent data.
        ///
        /// Packing extents in this method requires that `attributes` defines a size greater than 0.
        ///
        /// An implementation that doesn't supply the extents can ignore the packer and call the corresponding method in the ``FSVolume/Handler`` protocol, ``FSVolume/Handler/createItem(named:type:in:attributes:context:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - name: The new file's name.
        ///   - directory: The directory in which to create the file.
        ///   - newAttributes: Attributes to apply to the new file.
        ///   - packer: An extent packer you use to pack the file's allocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateFileKOIOResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, the volume's update free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateFileKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createFile(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateFileKOIOResult?, (any Error)?) -> Void)

        /// Creates a new file item and map its disk space.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// Only perform this technique opportunistically.
        /// In particular, don't perform additional I/O to fetch extent data.
        ///
        /// Packing extents in this method requires that `attributes` defines a size greater than 0.
        ///
        /// An implementation that doesn't supply the extents can ignore the packer and call the corresponding method in the ``FSVolume/Handler`` protocol, ``FSVolume/Handler/createItem(named:type:in:attributes:context:replyHandler:)``.
        ///
        /// - Parameters:
        ///   - name: The new file's name.
        ///   - directory: The directory in which to create the file.
        ///   - newAttributes: Attributes to apply to the new file.
        ///   - packer: An extent packer you use to pack the file's allocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If creation succeeds, pass an instance of ``FSCreateFileKOIOResult`` containing the newly-created ``FSItem``, its ``FSFileName``, its ``FSItemAttributes``, the updated ``FSItemAttributes`` of the parent directory, the volume's update free space, along with a `nil` error. If creation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSCreateFileKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func createFile(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, packer: FSExtentPacker, context: FSContext) async throws -> FSCreateFileKOIOResult

        /// Looks up an item within a directory and maps its disk space.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// Only perform this technique opportunistically.
        /// In particular, don't perform additional I/O to fetch extent data.
        ///
        /// - Parameters:
        ///   - name: The name of the file to look up.
        ///   - directory: The directory in which to look up the file.
        ///   - packer: An extent packer you use to pack the file's allocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemKOIOResult`` containing the found ``FSItem`` together with its ``FSFileName`` (as saved within the file system) and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemKOIOResult?, (any Error)?) -> Void)

        /// Looks up an item within a directory and maps its disk space.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        /// Only perform this technique opportunistically.
        /// In particular, don't perform additional I/O to fetch extent data.
        ///
        /// - Parameters:
        ///   - name: The name of the file to look up.
        ///   - directory: The directory in which to look up the file.
        ///   - packer: An extent packer you use to pack the file's allocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If lookup succeeds, pass an instance of ``FSLookupItemKOIOResult`` containing the found ``FSItem`` together with its ``FSFileName`` (as saved within the file system) and its ``FSItemAttributes``, along with a `nil` error. If lookup fails, pass the relevant error as the second parameter; FSKit ignores the ``FSLookupItemKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        func lookupItem(named name: FSFileName, in directory: FSItem, packer: FSExtentPacker, context: FSContext) async throws -> FSLookupItemKOIOResult

        /// Preallocates and maps disk space for the given file.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        ///
        /// > Important: Only implement this method if your file system conforms to ``FSVolume/PreallocateHandler``.
        ///
        /// - Parameters:
        ///   - file: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - packer: An extent packer you use to pack the file's preallocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateKOIOResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file, the volume's update free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateKOIOResult?, (any Error)?) -> Void)

        /// Preallocates and maps disk space for the given file.
        ///
        /// This method allows the module to opportunistically supply extents, avoiding future calls to ``blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
        ///
        /// > Important: Only implement this method if your file system conforms to ``FSVolume/PreallocateHandler``.
        ///
        /// - Parameters:
        ///   - file: The item for which to preallocate space.
        ///   - offset: The offset from which to allocate.
        ///   - length: The length of the space in bytes.
        ///   - flags: Flags that affect the preallocation behavior.
        ///   - packer: An extent packer you use to pack the file's preallocated disk space.
        ///   - context: An object that enables context-aware file system decisions throughout the operation.
        ///   - reply: A block or closure to indicate success or failure. If preallocation succeeds, pass an instance of ``FSPreallocateKOIOResult`` containing the amount of bytes allocated, the updated ``FSItemAttributes`` of the file, the volume's update free space, along with a `nil` error. If preallocation fails, pass the relevant error as the second parameter; FSKit ignores the ``FSPreallocateKOIOResult`` instance in this case. For an `async` Swift implementation, there's no reply handler; simply return the result instance or throw an error.
        optional func preallocateSpace(for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker, context: FSContext) async throws -> FSPreallocateKOIOResult
    }
}

/// An abstract base class for all result objects in FSKit handler-style protocols.
///
/// This class provides the common functionality needed for all result objects. All specialized result classes inherit from this base class.
@available(macOS 27.0, *)
open class FSVolumeHandlerResult : NSObject {

    /// A set of attributes to populate.
    ///
    /// Your module populates these attributes in ``FSItem/Attributes`` instances.
    ///
    /// Different operations may require different attribute sets. Access this property through the relevant result subclass, such as ``FSLookupItemResult.requestedAttributes`.
    ///
    /// > Important: Be sure to populate all requested attributes. FSKit caches all populated attributes and may use them in subsequent operations, even if not explicitly requested.
    open class var requestedAttributes: FSItem.GetAttributesRequest { get }
}

/// The result of an activate call.
///
/// Use this type in your implementation of ``FSVolume/Handler/activateVolume(options:replyHandler:)``.
@available(macOS 27.0, *)
open class FSActivateResult : FSVolumeHandlerResult {

    /// Creates an activate result instance.
    ///
    /// - Parameters:
    ///   - rootItem: The root ``FSItem`` of the volume.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(rootItem: FSItem)
}

/// The result of an item lookup call.
///
///  Use this type in your implementation of ``FSVolume/Handler/lookupItem(named:in:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSLookupItemResult : FSVolumeHandlerResult {

    /// Creates a result for an item-lookup operation.
    ///
    /// - Parameters:
    ///   - foundItem: The ``FSItem`` found by the directory lookup.
    ///   - itemName: The item's name as it exists within the file system. The value may differ from the requested name in order to handle case-insensitive file systems or Unicode normalization.
    ///   - itemAttributes: The ``FSItemAttributes`` of the found item.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(foundItem: FSItem, itemName: FSFileName, itemAttributes: FSItem.Attributes)
}

/// The result of a create-item call.
///
/// Use this type in your implementation of ``FSVolume/Handler/createItem(named:type:in:attributes:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSCreateItemResult : FSVolumeHandlerResult {

    /// Creates a result for an item-creation operation.
    ///
    /// - Parameters:
    ///   - newItem: The newly-created ``FSItem``.
    ///   - newItemName: The name of the newly-created item as it exists within the file system.
    ///   - newItemAttributes: The ``FSItemAttributes`` of the newly-created item.
    ///   - directoryAttributes: The updated ``FSItemAttributes`` of the parent directory.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation completes, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(newItem: FSItem, newItemName: FSFileName, newItemAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of a create-symlink call.
///
/// Use this type in your implementation of  ``FSVolume/Handler/createSymbolicLink(named:in:attributes:linkContents:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSCreateSymlinkResult : FSCreateItemResult {
}

/// The result of a create-link call.
///
/// Use this type in your implementation of ``FSVolume/Handler/createLink(to:named:in:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSCreateLinkResult : FSVolumeHandlerResult {

    /// Creates a result for a link-creation operation.
    ///
    /// - Parameters:
    ///   - linkName: The name of the newly-created hard link.
    ///   - linkAttributes: The ``FSItemAttributes`` of the linked item (the target of the hard link).
    ///   - directoryAttributes: The updated ``FSItemAttributes`` of the parent directory.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(linkName: FSFileName, linkAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of a rename-item call.
///
/// Use this type in your implementation of  ``FSVolume/Handler/renameItem(_:inDirectory:named:to:inDirectory:overItem:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSRenameItemResult : FSVolumeHandlerResult {

    /// Creates a result for an item-renaming operation.
    ///
    /// - Parameters:
    ///   - newName: The updated item name as it exists within the destination directory.
    ///   - renamedItemAttributes: The ``FSItemAttributes`` of the renamed item.
    ///   - sourceDirectoryAttributes: The updated ``FSItemAttributes`` of the source directory.
    ///   - destinationDirectoryAttributes: The updated ``FSItemAttributes`` of the destination directory.
    ///   - overItemAttributes: The ``FSItemAttributes`` of the overwritten item, if any. Pass `nil` if the action didn't overwrite any item.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(newName: FSFileName, renamedItemAttributes: FSItem.Attributes, sourceDirectoryAttributes: FSItem.Attributes, destinationDirectoryAttributes: FSItem.Attributes, overItemAttributes: FSItem.Attributes?, freeSpace: FSFreeSpace?)
}

/// The result of a remove-item call.
///
/// Use this type in your implementation of``FSVolume/Handler/removeItem(_:named:from:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSRemoveItemResult : FSVolumeHandlerResult {

    /// Creates a result for an item-removal operation.
    ///
    /// - Parameters:
    ///   - itemAttributes: The ``FSItemAttributes`` of the removed item.
    ///   - directoryAttributes: The updated ``FSItemAttributes`` of the parent directory.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation finishes, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(itemAttributes: FSItem.Attributes, directoryAttributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of a get-attributes call.
///
/// Use this type in your implementation of ``FSVolume/Handler/getAttributes(_:of:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSGetAttributesResult : FSVolumeHandlerResult {

    /// Creates a result for an attribute-getting operation.
    ///
    /// - Parameters:
    ///   - attributes: The requested ``FSItemAttributes`` for the item.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(attributes: FSItem.Attributes)
}

/// The restlt of a set-attributes call.
///
/// Use this type in your implementation of ``FSVolume/Handler/setAttributes(_:on:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSSetAttributesResult : FSVolumeHandlerResult {

    /// Creates a result for an attribute-setting operation.
    ///
    /// - Parameters:
    ///   - attributes: The updated ``FSItemAttributes`` for the item.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of an enumerate-directory call.
///
/// Use this type in your implementation of ``FSVolume/Handler/enumerateDirectory(_:startingAt:verifier:attributes:packer:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSEnumerateDirectoryResult : FSVolumeHandlerResult {

    /// Creates a result for an directory enumeration operation.
    ///
    /// - Parameters:
    ///   - currentVerifier: An `FSDirectoryVerifier` value that reflects the directory's current version. FSKit uses this value to detect whether the directory contents changed since the last enumeration call.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(verifier currentVerifier: UInt64)
}

/// The result of a read-symlink call.
///
/// Use this type in your implementation of ``FSVolume/Handler/readSymbolicLink(_:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSReadSymlinkResult : FSVolumeHandlerResult {

    /// Creates a result for a symlink-reading operation.
    ///
    /// - Parameters:
    ///   - contents: The contents of the symbolic link.
    ///   - attributes: The ``FSItemAttributes`` of the symbolic link.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(contents: FSFileName, symlinkAttributes attributes: FSItem.Attributes)
}

/// The result of a get-extended-attributes call.
///
/// Use this type in your implementation ``FSVolume/XattrHandler/getXattr(named:of:context:replyHandler:)``
@available(macOS 27.0, *)
open class FSGetXattrResult : FSVolumeHandlerResult {

    /// Creates a result for an extended-attribute-getting operation.
    ///
    /// - Parameters:
    ///   - value: The extended attribute value for the requested attribute name.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(xattrValue value: Data)
}

/// The result of a set-extended-attributes call.
///
/// Use this type in your implementation of  ``FSVolume/XattrHandler/setXattr(named:to:on:policy:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSSetXattrResult : FSVolumeHandlerResult {

    /// Creates a result for an extended-attribute-setting operation.
    ///
    /// - Parameters:
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(freeSpace: FSFreeSpace?)
}

/// The result of a list-extended-attributes call.
///
/// Use this type in your implementation of ``FSVolume/XattrHandler/supportedXattrNames(for:)``.
@available(macOS 27.0, *)
open class FSListXattrsResult : FSVolumeHandlerResult {

    /// Creates a result instance with all required properties populated.
    ///
    /// - Parameters:
    ///   - xattrNames: An array of ``FSFileName`` instances representing the names of all extended attributes currently set on the item.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(xattrNames: [FSFileName])
}

/// The result of a read-file call.
///
/// Use this type in your implementation of ``FSVolume/ReadWriteHandler/read(from:at:length:into:replyHandler:)``.
@available(macOS 27.0, *)
open class FSReadFileResult : FSVolumeHandlerResult {

    /// Creates a result for a file-reading operation.
    ///
    /// - Parameters:
    ///   - actuallyRead: The number of bytes actually read from the file. This may be less than the requested length if the end of file was reached.
    ///   - attributes: The updated ``FSItemAttributes`` of the file after the read operation (e.g., updated access time).
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(bytesRead actuallyRead: Int, itemAttributes attributes: FSItem.Attributes)
}

/// The result of a read-file call.
///
/// Use this type in your implementation of ``FSVolume/ReadWriteHandler/write(contents:to:at:replyHandler:)``.
@available(macOS 27.0, *)
open class FSWriteFileResult : FSVolumeHandlerResult {

    /// Creates a result for a file-writing operation.
    ///
    /// - Parameters:
    ///   - actuallyWritten: The number of bytes actually written to the file. This should match the requested write length unless an error occurred.
    ///   - attributes: The updated ``FSItemAttributes`` of the file after the write operation. Examples of attributes you might need to update are the updated file size and modification time.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(bytesWritten actuallyWritten: Int, itemAttributes attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of a check-access call.
///
/// Use this type in your implementation of ``FSVolume/AccessCheckHandler/checkAccess(to:requestedAccess:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSCheckAccessResult : FSVolumeHandlerResult {

    /// Creates a result for an access-checking operation.
    ///
    /// - Parameters:
    ///   - accessAllowed: A Boolean value indicating whether the file system grants the requested access to the item. Pass `true` (Swift) or `YES` (Obj-C) to allow access, `false` (Swift) or `NO` (Obj-C) to deny access.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(accessAllowed: Bool)
}

/// The result of a rename-volume call.
///
///  Use this type in your implementation of ``FSVolume/RenameHandler/setVolumeName(_:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSVolumeRenameResult : FSVolumeHandlerResult {

    /// Creates a result for a volume-renaming operation.
    ///
    /// - Parameters:
    ///   - newName: The new volume name.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(newName: FSFileName)
}

/// The result of a preallocate call.
///
/// Use this type in your implementation of  ``FSVolume/PreallocateHandler/preallocateSpace(for:at:length:flags:context:replyHandler:)``
@available(macOS 27.0, *)
open class FSPreallocateResult : FSVolumeHandlerResult {

    /// Creates a result for a preallocate operation.
    ///
    /// - Parameters:
    ///   - bytesAllocated: The number of bytes actually allocated for the file during the preallocation operation.
    ///   - attributes: The updated ``FSItemAttributes`` of the file after the preallocation operation.
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(bytesAllocated: Int, itemAttributes attributes: FSItem.Attributes, freeSpace: FSFreeSpace?)
}

/// The result of a deactivate-item call.
///
/// Use this type in your implementation of ``FSVolume/ItemDeactivationHandler/deactivateItem(_:context:replyHandler:)``
@available(macOS 27.0, *)
open class FSDeactivateItemResult : FSVolumeHandlerResult {

    /// Creates a result for an item-deactivation operation.
    ///
    /// - Parameters:
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(freeSpace: FSFreeSpace?)
}

/// A seek-region result.
///
/// Use this type in your implementation of ``FSVolume/SeekRegionHandler/seek(within:from:region:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSSeekRegionResult : FSVolumeHandlerResult {

    /// Creates a result for a region-seeking operation.
    ///
    /// - Parameters:
    ///   - returnedOffset: The offset of the requested region, greater than or equal to the supplied offset.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init(returnedOffset: off_t)
}

/// The result of a blockmap call.
///
/// Use this type in your implementation of ``FSVolume/KernelOffloadedIOHandler/blockmapFile(_:offset:length:flags:operationID:packer:replyHandler:)``.
@available(macOS 27.0, *)
open class FSBlockmapResult : FSVolumeHandlerResult {

    /// Creates a result for a blockmap operation.
    ///
    /// - Parameters:
    ///   - freeSpace: An ``FSFreeSpace`` instance populated with the volume's updated free space. Passing a `nil` free space causes FSKit to calculate the free space when the operation is done, based on the volume's ``FSVolume/Handler/volumeStatistics`` property. This behavior may lead to degraded performance.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(freeSpace: FSFreeSpace?)
}

/// The result of a complete-I/O call.
///
/// Use this type in your implementation of  ``FSVolume/KernelOffloadedIOHandler/completeIO(for:offset:length:status:flags:operationID:replyHandler:)``
@available(macOS 27.0, *)
open class FSCompleteIOResult : FSVolumeHandlerResult {

    /// Creates a result for an I/O-completion operation.
    ///
    /// - Parameters:
    ///   - attributes: The updated ``FSItemAttributes`` of the file after the I/O completion operation (e.g., updated size, modification time).
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init?(itemAttributes attributes: FSItem.Attributes)
}

/// The result of a kernel-offloaded create-file call.
///
/// Use this type in your implementation of ``FSVolume/KernelOffloadedIOHandler/createFile(named:in:attributes:packer:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSCreateFileKOIOResult : FSCreateItemResult {
}

/// The result of a kernel-offloaded lookup-item call.
///
/// Use this type in your implementation of ``FSVolume/KernelOffloadedIOHandler/lookupItem(named:in:packer:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSLookupItemKOIOResult : FSLookupItemResult {
}

/// The result of a kernel-offloaded preallocate call.
///
/// Use this type in your implementation of ``FSVolume/KernelOffloadedIOHandler/preallocateSpace(for:at:length:flags:packer:context:replyHandler:)``
@available(macOS 27.0, *)
open class FSPreallocateKOIOResult : FSPreallocateResult {
}

/// The result of an open-item call.
///
/// Use this type in your implementation of ``FSVolume/DataCacheHandler/open(_:modes:cacheMode:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSOpenItemResult : FSVolumeHandlerResult {

    /// Creates an open-item result.
    ///
    /// - Parameters:
    ///   - grantedCoherency: The ``FSKernelCacheCoherencyType`` granted by the module for this item.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init(grantedCoherency: FSVolume.KernelCacheCoherencyType)
}

/// The result of an upgrade-item call.
///
/// Use this type in your implementation of ``FSVolume/DataCacheHandler/upgrade(_:cacheMode:context:replyHandler:)``.
@available(macOS 27.0, *)
open class FSUpgradeItemResult : FSVolumeHandlerResult {

    /// Creates an upgrade-item result.
    ///
    /// - Parameters:
    ///   - grantedCoherency: The ``FSKernelCacheCoherencyType`` granted by the module after the upgrade.
    /// - Returns: A populated result instance, or `nil` if validation fails.
    public init(grantedCoherency: FSVolume.KernelCacheCoherencyType)
}

/// A protocol for implementing a minimal file system as an app extension.
///
/// Your app needs to do the following to implement a FSKit-compatible minimal file system:
///
/// 1. Create a subclass of ``FSUnaryFileSystem``, which also conforms to ``FSUnaryFileSystemOperations``.
/// 2. Implement a `@main` struct that conforms to the `UnaryFileSystemExtension` protocol. Your implementation of this protocol returns the type of class from step 1 as its ``FileSystem`` associated type, and returns an instance of it as the ``fileSystem-swift.property`` property.
@available(macOS 15.4, *)
@available(iOS, unavailable)
@available(watchOS, unavailable)
@available(tvOS, unavailable)
@available(visionOS, unavailable)
public protocol UnaryFileSystemExtension : AppExtension {

    /// The type of file system your app extension provides.
    associatedtype FileSystem : FSUnaryFileSystem, FSUnaryFileSystemOperations

    /// The instance of your file system type that your app extension provides.
    var fileSystem: Self.FileSystem { get }
}

@available(macOS 15.4, *)
@available(iOS, unavailable)
@available(watchOS, unavailable)
@available(tvOS, unavailable)
@available(visionOS, unavailable)
extension UnaryFileSystemExtension {

    public var configuration: some AppExtensionConfiguration { get }
}

