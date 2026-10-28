import FileProvider
import UniformTypeIdentifiers

enum ItemID {
    static let serverRoot = "root"
    /// Items moved to basebook's Trash from this Mac (Finder's Trash).
    static let serverTrash = "trash"

    static func server(_ id: NSFileProviderItemIdentifier) -> String? {
        switch id {
        case .rootContainer: return serverRoot
        case .trashContainer: return serverTrash
        case .workingSet: return nil
        default: return id.rawValue
        }
    }

    static func system(_ id: String) -> NSFileProviderItemIdentifier {
        switch id {
        case serverRoot: return .rootContainer
        case serverTrash: return .trashContainer
        default: return NSFileProviderItemIdentifier(id)
        }
    }

    static func isShareRoot(_ id: String) -> Bool { id.hasPrefix("root:") }
}

enum Decoration {
    static let disconnectedBadge = NSFileProviderItemDecorationIdentifier("dev.vogel.homeport.HomeportFP.disconnected")
    static let disconnectedLabel = NSFileProviderItemDecorationIdentifier("dev.vogel.homeport.HomeportFP.disconnectedLabel")
}

private func versionData(_ v: UInt64, _ extra: UInt8? = nil) -> Data {
    var be = v.bigEndian
    var d = Data(bytes: &be, count: 8)
    if let extra { d.append(extra) }
    return d
}

/// Fields the server does not store, echoed back to the system after a
/// change so it does not see them as rejected.
struct ItemEcho {
    var lastUsedDate: Date?
    var symlinkTarget: String?

    init(from item: NSFileProviderItem) {
        lastUsedDate = item.lastUsedDate ?? nil
        symlinkTarget = item.symlinkTargetPath ?? nil
    }
}

/// An item backed by a server item.
final class Item: NSObject, NSFileProviderItem, NSFileProviderItemDecorating {
    let raw: FfiItem
    /// Root and share roots carry the link state as a decoration.
    let offline: Bool?
    let echo: ItemEcho?

    init(_ raw: FfiItem, offline: Bool? = nil, echo: ItemEcho? = nil) {
        self.raw = raw
        self.offline = offline
        self.echo = echo
    }

    static func root(offline: Bool?) -> Item {
        Item(
            FfiItem(
                id: ItemID.serverRoot, parentId: ItemID.serverRoot, name: HomeportConfig.domainDisplayName,
                kind: .dir, size: 0, mtimeNs: 0, ctimeNs: 0, mode: 0o555, rev: 1, contentRev: 1,
                symlinkTarget: nil, crtimeNs: 0, flags: 0, tags: nil),
            offline: offline)
    }

    var itemIdentifier: NSFileProviderItemIdentifier { ItemID.system(raw.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { ItemID.system(raw.parentId) }
    var filename: String { raw.name }

    var contentType: UTType {
        switch raw.kind {
        case .dir: return .folder
        case .symlink: return .symbolicLink
        case .other: return .item
        case .file:
            let ext = (raw.name as NSString).pathExtension
            return ext.isEmpty ? .data : (UTType(filenameExtension: ext) ?? .data)
        }
    }

    var capabilities: NSFileProviderItemCapabilities {
        if raw.id == ItemID.serverRoot { return [.allowsReading, .allowsContentEnumerating] }
        if ItemID.isShareRoot(raw.id) { return [.allowsReading, .allowsContentEnumerating, .allowsAddingSubItems] }
        if raw.parentId == ItemID.serverTrash {
            // Put Back, or delete from the Trash (it stays in basebook's Trash).
            var c: NSFileProviderItemCapabilities = [.allowsReading, .allowsReparenting, .allowsDeleting]
            if raw.kind == .dir { c.insert(.allowsContentEnumerating) }
            return c
        }
        let common: NSFileProviderItemCapabilities = [.allowsReading, .allowsRenaming, .allowsReparenting, .allowsTrashing, .allowsDeleting]
        switch raw.kind {
        case .dir: return common.union([.allowsContentEnumerating, .allowsAddingSubItems])
        case .file: return common.union(.allowsWriting)
        default: return common
        }
    }

    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = [.userReadable]
        if raw.mode & 0o200 != 0 { f.insert(.userWritable) }
        if raw.kind == .dir || raw.mode & 0o100 != 0 { f.insert(.userExecutable) }
        if raw.flags & 0x8000 != 0 { f.insert(.hidden) }
        return f
    }

    var documentSize: NSNumber? { raw.kind == .file ? NSNumber(value: raw.size) : nil }

    var contentModificationDate: Date? {
        raw.mtimeNs == 0 ? nil : Date(timeIntervalSince1970: Double(raw.mtimeNs) / 1e9)
    }

    var creationDate: Date? {
        raw.crtimeNs == 0 ? nil : Date(timeIntervalSince1970: Double(raw.crtimeNs) / 1e9)
    }

    var symlinkTargetPath: String? { raw.symlinkTarget ?? echo?.symlinkTarget }

    var tagData: Data? { raw.tags.flatMap(TagCodec.tagData(fromXattr:)) }

    var lastUsedDate: Date? { echo?.lastUsedDate }


    var itemVersion: NSFileProviderItemVersion {
        let meta = versionData(raw.rev, offline.map { $0 ? 1 : 0 })
        return NSFileProviderItemVersion(contentVersion: versionData(raw.contentRev), metadataVersion: meta)
    }

    var decorations: [NSFileProviderItemDecorationIdentifier]? {
        offline == true ? [Decoration.disconnectedBadge, Decoration.disconnectedLabel] : nil
    }
}

/// Finder tags travel as the xattr `com.apple.metadata:_kMDItemUserTags` on
/// basebook (a plist array of "Name" or "Name\nColor" strings) but as an
/// NSKeyedArchiver dictionary `{v: 1, t: [[Name, 1], ...]}` in
/// NSFileProviderItem.tagData.
enum TagCodec {
    static func names(fromXattr data: Data) -> [String]? {
        guard let list = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String] else { return nil }
        return list.map { $0.split(separator: "\n", maxSplits: 1).first.map(String.init) ?? $0 }
    }

    static func tagData(fromXattr data: Data) -> Data? {
        guard let names = names(fromXattr: data), !names.isEmpty else { return nil }
        let dict: NSDictionary = ["v": 1, "t": names.map { [$0, 1] as NSArray } as NSArray]
        return try? NSKeyedArchiver.archivedData(withRootObject: dict, requiringSecureCoding: false)
    }

    /// The xattr value for `tagData` (nil: no tags).
    static func xattr(fromTagData data: Data) -> Data? {
        guard let u = try? NSKeyedUnarchiver(forReadingFrom: data) else { return nil }
        u.requiresSecureCoding = false
        guard let dict = u.decodeObject(forKey: NSKeyedArchiveRootObjectKey) as? NSDictionary,
              let tags = dict["t"] as? [NSArray] else { return nil }
        let names = tags.compactMap { $0.firstObject as? String }
        guard !names.isEmpty else { return nil }
        return try? PropertyListSerialization.data(fromPropertyList: names, format: .binary, options: 0)
    }
}
