import Foundation

/// Settings shared by the menu bar app and the File Provider extension,
/// stored as `config.json` in the app group container.
struct HomeportConfig: Codable, Equatable {
    static let appGroup = "9G68SMNHEU.dev.vogel.homeport"
    static let domainIdentifier = "basebook"
    static let domainDisplayName = "basebook"
    static let defaultServer = "100.84.220.87:47820"

    var server: String
    var fingerprint: String
    /// Stable per client machine; the server filters its change feed by it.
    var clientId: String

    static var containerURL: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
    }

    static var configURL: URL? { containerURL?.appendingPathComponent("config.json") }

    /// The extension's metadata cache.
    static var cacheURL: URL? { containerURL?.appendingPathComponent("Cache/metadata.sqlite") }

    static func load() -> HomeportConfig? {
        guard let url = configURL, let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(HomeportConfig.self, from: data)
    }

    func save() throws {
        guard let url = Self.configURL else { throw CocoaError(.fileNoSuchFile) }
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try enc.encode(self).write(to: url, options: .atomic)
    }
}
