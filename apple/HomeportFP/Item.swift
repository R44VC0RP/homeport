import FileProvider
import UniformTypeIdentifiers

enum ItemID {
    static let serverRoot = "root"

    static func server(_ id: NSFileProviderItemIdentifier) -> String? {
        switch id {
        case .rootContainer: return serverRoot
        case .trashContainer, .workingSet: return nil
        default: return id.rawValue
        }
    }

    static func system(_ id: String) -> NSFileProviderItemIdentifier {
        id == serverRoot ? .rootContainer : NSFileProviderItemIdentifier(id)
    }
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

/// A read-only item backed by a server item.
final class Item: NSObject, NSFileProviderItem, NSFileProviderItemDecorating {
    let raw: FfiItem
    /// Root and share roots carry the link state as a decoration.
    let offline: Bool?

    init(_ raw: FfiItem, offline: Bool? = nil) {
        self.raw = raw
        self.offline = offline
    }

    static func root(offline: Bool?) -> Item {
        Item(
            FfiItem(
                id: ItemID.serverRoot, parentId: ItemID.serverRoot, name: HomeportConfig.domainDisplayName,
                kind: .dir, size: 0, mtimeNs: 0, ctimeNs: 0, mode: 0o555, rev: 1, contentRev: 1,
                symlinkTarget: nil),
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
        switch raw.kind {
        case .dir: return [.allowsReading, .allowsContentEnumerating]
        default: return [.allowsReading]
        }
    }

    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = [.userReadable]
        if raw.kind == .dir || raw.mode & 0o100 != 0 { f.insert(.userExecutable) }
        return f
    }

    var documentSize: NSNumber? { raw.kind == .file ? NSNumber(value: raw.size) : nil }

    var contentModificationDate: Date? {
        raw.mtimeNs == 0 ? nil : Date(timeIntervalSince1970: Double(raw.mtimeNs) / 1e9)
    }

    var creationDate: Date? {
        raw.ctimeNs == 0 ? nil : Date(timeIntervalSince1970: Double(raw.ctimeNs) / 1e9)
    }

    var symlinkTargetPath: String? { raw.symlinkTarget }

    var itemVersion: NSFileProviderItemVersion {
        let meta = versionData(raw.rev, offline.map { $0 ? 1 : 0 })
        return NSFileProviderItemVersion(contentVersion: versionData(raw.contentRev), metadataVersion: meta)
    }

    var decorations: [NSFileProviderItemDecorationIdentifier]? {
        offline == true ? [Decoration.disconnectedBadge, Decoration.disconnectedLabel] : nil
    }
}
