import Foundation

/// What a server can do (`homeport_proto::caps` bits).
struct ServerCaps: OptionSet, Codable, Equatable {
    let rawValue: UInt32
    static let thumbnails = ServerCaps(rawValue: 1 << 0)
    static let trash = ServerCaps(rawValue: 1 << 1)
    static let xattrs = ServerCaps(rawValue: 1 << 2)
    static let tags = ServerCaps(rawValue: 1 << 3)
    static let symlinks = ServerCaps(rawValue: 1 << 4)
    static let setCrtime = ServerCaps(rawValue: 1 << 5)
    static let hidden = ServerCaps(rawValue: 1 << 6)
    static let mode = ServerCaps(rawValue: 1 << 7)
    /// Assumed until the server has said otherwise (what 0.5 did).
    static let all = ServerCaps(rawValue: 0xff)
}

/// Bits of `FfiItem.flags` (`homeport_proto::item_flags`).
enum ItemFlags {
    static let readOnly: UInt32 = 0x0000_0002
    static let hidden: UInt32 = 0x0000_8000
    static let nameAliased: UInt32 = 0x0100_0000
    static let nameUnrepresentable: UInt32 = 0x0200_0000
    static let cloudOnly: UInt32 = 0x4000_0000
}

/// The last `ServerInfo` a server sent, kept so the extension knows its
/// capabilities before (or without) connecting.
struct StoredServerInfo: Codable, Equatable {
    var name: String
    var os: String
    var version: String
    var caseSensitive: Bool
    var normalizationSensitive: Bool
    var caps: UInt32

    init(_ i: FfiServerInfo) {
        name = i.name
        os = i.os
        version = i.version
        caseSensitive = i.caseSensitive
        normalizationSensitive = i.normalizationSensitive
        caps = i.caps
    }
}

/// One paired server: a File Provider domain and its sidebar item.
struct ServerEntry: Codable, Equatable, Identifiable {
    /// The File Provider domain identifier. Never changes for an entry.
    var id: String
    /// Shown in Finder's sidebar.
    var name: String
    /// `ip:port` of homeportd.
    var address: String
    /// Hex SHA-256 of the server certificate (pinned).
    var fingerprint: String
    /// Stable per client and server: the server keys its change-feed filter on it.
    var clientId: String
    /// The extension's metadata cache, relative to the app group container.
    var cachePath: String
    var info: StoredServerInfo?

    var caps: ServerCaps { info.map { ServerCaps(rawValue: $0.caps) } ?? .all }

    /// The domain identifier for a server with this certificate: removing
    /// and adding the same server again gives the same domain.
    static func domainId(fingerprint: String) -> String {
        "hp-" + fingerprint.lowercased().prefix(16)
    }
}

/// The server list (`servers.json` in the app group container), shared by
/// the menu bar app and the File Provider extension.
enum ServerStore {
    static let appGroup = "9G68SMNHEU.dev.vogel.homeport"

    static var containerURL: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
    }

    static var listURL: URL? { containerURL?.appendingPathComponent("servers.json") }

    /// 0.5 and earlier: one server in `config.json`, domain `basebook`.
    private struct LegacyConfig: Codable {
        var server: String
        var fingerprint: String
        var clientId: String
    }

    static let legacyDomainId = "basebook"
    static let legacyCachePath = "Cache/metadata.sqlite"

    static func cacheURL(for entry: ServerEntry) -> URL? {
        containerURL?.appendingPathComponent(entry.cachePath)
    }

    /// The servers. Before the first save, a 0.5 `config.json` is read as
    /// one server with its old domain and cache, so nothing re-downloads.
    static func load() -> [ServerEntry] {
        if let url = listURL, let data = try? Data(contentsOf: url),
           let list = try? JSONDecoder().decode([ServerEntry].self, from: data) {
            return list
        }
        return legacy().map { [$0] } ?? []
    }

    static func legacy() -> ServerEntry? {
        guard let url = containerURL?.appendingPathComponent("config.json"), let data = try? Data(contentsOf: url),
              let c = try? JSONDecoder().decode(LegacyConfig.self, from: data) else { return nil }
        return ServerEntry(id: legacyDomainId, name: "basebook", address: c.server, fingerprint: c.fingerprint,
                           clientId: c.clientId, cachePath: legacyCachePath, info: nil)
    }

    static func save(_ list: [ServerEntry]) throws {
        guard let url = listURL else { throw CocoaError(.fileNoSuchFile) }
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try enc.encode(list).write(to: url, options: .atomic)
    }
}
