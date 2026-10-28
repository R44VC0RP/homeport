import FileProvider
import os

let log = Logger(subsystem: "dev.vogel.homeport", category: "fp")

/// Maps core errors to what the File Provider system understands.
func fpError(_ error: Error) -> NSError {
    guard let e = error as? CoreError else { return error as NSError }
    switch e {
    case .Unreachable:
        return NSFileProviderError(.serverUnreachable) as NSError
    case .NotFound:
        return NSFileProviderError(.noSuchItem) as NSError
    case .AnchorExpired:
        return NSFileProviderError(.syncAnchorExpired) as NSError
    case .Cancelled:
        return NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)
    case .Server(let msg), .Io(let msg), .Config(let msg):
        return NSError(domain: NSCocoaErrorDomain, code: NSFileReadUnknownError,
                       userInfo: [NSLocalizedDescriptionKey: msg])
    }
}

/// Read-only error for writes in this phase.
private func readOnlyError() -> NSError {
    NSFileProviderError(.cannotSynchronize) as NSError
}

private final class ProgressForwarder: ProgressSink, @unchecked Sendable {
    let progress: Progress
    init(_ p: Progress) { progress = p }
    func progress(done: UInt64) {
        progress.completedUnitCount = Int64(done)
    }
}

final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderPartialContentFetching {
    let domain: NSFileProviderDomain
    let manager: NSFileProviderManager?
    let core: Core?

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        self.manager = NSFileProviderManager(for: domain)
        var core: Core?
        if let cfg = HomeportConfig.load() {
            do {
                core = try Core(config: CoreConfig(
                    server: cfg.server, fingerprint: cfg.fingerprint, clientId: cfg.clientId,
                    cachePath: HomeportConfig.cacheURL?.path, requestDeadlineMs: 800, workerThreads: 2))
            } catch {
                log.error("core init failed: \(String(describing: error), privacy: .public)")
            }
        } else {
            log.error("no config.json in the app group container; start Homeport.app")
        }
        self.core = core
        super.init()
        log.notice("extension started for domain \(domain.identifier.rawValue, privacy: .public)")
    }

    func invalidate() {
        log.notice("extension invalidated")
    }

    private let lock = NSLock()
    private var reportedUnreachable = false

    /// Called with the outcome of every server call: after the system has
    /// seen serverUnreachable, the first success tells it to retry.
    func noteOutcome(_ error: Error?) {
        let unreachable: Bool
        if let e = error as? CoreError, case .Unreachable = e { unreachable = true } else { unreachable = false }
        if error != nil && !unreachable { return }
        lock.lock()
        let wasUnreachable = reportedUnreachable
        reportedUnreachable = unreachable
        lock.unlock()
        if wasUnreachable && !unreachable {
            log.notice("server reachable again; resolving serverUnreachable")
            manager?.signalErrorResolved(NSFileProviderError(.serverUnreachable)) { _ in }
            signalWorkingSet()
        }
    }

    func signalWorkingSet() {
        manager?.signalEnumerator(for: .workingSet) { _ in }
    }

    /// Whether the link is known to be down (after the first probe).
    var isOffline: Bool {
        guard let link = core?.link() else { return true }
        return link.probed && link.state == .offline
    }

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let core else {
            completionHandler(nil, NSFileProviderError(.serverUnreachable))
            return progress
        }
        if identifier == .rootContainer {
            completionHandler(Item.root(offline: isOffline), nil)
            return progress
        }
        guard let id = ItemID.server(identifier) else {
            completionHandler(nil, NSFileProviderError(.noSuchItem))
            return progress
        }
        Task {
            do {
                let raw = try await core.item(id: id)
                let shareRoot = raw.parentId == ItemID.serverRoot
                completionHandler(Item(raw, offline: shareRoot ? self.isOffline : nil), nil)
            } catch {
                completionHandler(nil, fpError(error))
            }
            progress.completedUnitCount = 1
        }
        return progress
    }

    private func tempURL() throws -> URL {
        guard let manager else { throw NSFileProviderError(.providerNotFound) }
        return try manager.temporaryDirectoryURL().appendingPathComponent(UUID().uuidString)
    }

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 0)
        guard let core, let id = ItemID.server(itemIdentifier) else {
            completionHandler(nil, nil, NSFileProviderError(.noSuchItem))
            return progress
        }
        let token = CancelToken()
        progress.cancellationHandler = { token.cancel() }
        Task {
            let started = Date()
            do {
                var raw = try await core.item(id: id)
                progress.totalUnitCount = Int64(raw.size)
                let url = try self.tempURL()
                let done = try await core.readToFile(id: id, path: url.path, offset: 0, len: 0,
                                                     progress: ProgressForwarder(progress), cancel: token)
                raw.size = done.size
                raw.contentRev = done.contentRev
                self.noteOutcome(nil)
                log.notice("fetched \(id, privacy: .public) \(done.bytes) bytes in \(Int(Date().timeIntervalSince(started) * 1000)) ms")
                completionHandler(url, Item(raw), nil)
            } catch {
                self.noteOutcome(error)
                log.error("fetch \(id, privacy: .public) failed after \(Int(Date().timeIntervalSince(started) * 1000)) ms: \(String(describing: error), privacy: .public)")
                completionHandler(nil, nil, fpError(error))
            }
        }
        return progress
    }

    /// Files at least this large are fetched in pieces when the system asks for a range.
    private static let partialThreshold: UInt64 = 16 << 20
    private static let partialChunk: UInt64 = 4 << 20
    private static let partialMaxChunk: UInt64 = 256 << 20
    /// Per item: where the last range fetch ended and how long it was. A
    /// request that continues it is a sequential read, so the next piece is
    /// four times larger (each piece costs the system a round trip of its own).
    private var lastRange: [String: (end: UInt64, len: UInt64)] = [:]

    private func pieceLength(id: String, start: UInt64) -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        guard let last = lastRange[id], last.end == start else { return Self.partialChunk }
        return min(last.len * 4, Self.partialMaxChunk)
    }

    private func notePiece(id: String, start: UInt64, len: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        if lastRange.count > 64 { lastRange.removeAll() }
        lastRange[id] = (start + len, len)
    }

    func fetchPartialContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion,
                              request: NSFileProviderRequest, minimalRange requestedRange: NSRange, aligningTo alignment: Int,
                              options: NSFileProviderFetchContentsOptions,
                              completionHandler: @escaping (URL?, NSFileProviderItem?, NSRange, NSFileProviderMaterializationFlags, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 0)
        guard let core, let id = ItemID.server(itemIdentifier) else {
            completionHandler(nil, nil, NSRange(), [], NSFileProviderError(.noSuchItem))
            return progress
        }
        let token = CancelToken()
        progress.cancellationHandler = { token.cancel() }
        Task {
            let started = Date()
            do {
                var raw = try await core.item(id: id)
                let size = raw.size
                var offset: UInt64 = 0
                var len: UInt64 = 0
                if size >= Self.partialThreshold {
                    let align = UInt64(max(alignment, 1))
                    let start = UInt64(requestedRange.location) / align * align
                    let want = max(UInt64(requestedRange.location + requestedRange.length) - start,
                                   self.pieceLength(id: id, start: start))
                    var end = (start + want + align - 1) / align * align
                    if end >= size { end = size }
                    offset = start
                    len = end - start
                    self.notePiece(id: id, start: start, len: len)
                    if offset == 0 && end == size { len = 0 }
                }
                progress.totalUnitCount = Int64(len == 0 ? size - offset : len)
                let url = try self.tempURL()
                let done = try await core.readToFile(id: id, path: url.path, offset: offset, len: len,
                                                     progress: ProgressForwarder(progress), cancel: token)
                raw.size = done.size
                raw.contentRev = done.contentRev
                if options.contains(.strictVersioning),
                   requestedVersion.contentVersion != Item(raw).itemVersion.contentVersion {
                    try? FileManager.default.removeItem(at: url)
                    throw NSFileProviderError(.versionNoLongerAvailable)
                }
                let range = len == 0 ? NSRange(location: 0, length: Int(done.size))
                                     : NSRange(location: Int(offset), length: Int(done.bytes))
                self.noteOutcome(nil)
                log.notice("fetched \(id, privacy: .public) range \(range.location)+\(range.length) in \(Int(Date().timeIntervalSince(started) * 1000)) ms")
                completionHandler(url, Item(raw), range, [], nil)
            } catch {
                self.noteOutcome(error)
                log.error("partial fetch \(id, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                completionHandler(nil, nil, NSRange(), [], fpError(error))
            }
        }
        return progress
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, readOnlyError())
        return Progress()
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields,
                    contents newContents: URL?, options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, readOnlyError())
        return Progress()
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        // The system restores the item from the extension.
        completionHandler(NSFileProviderError(.deletionRejected))
        return Progress()
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        guard let core else { throw NSFileProviderError(.serverUnreachable) }
        if containerItemIdentifier == .workingSet {
            return WorkingSetEnumerator(ext: self, core: core)
        }
        guard let id = ItemID.server(containerItemIdentifier) else { throw NSFileProviderError(.noSuchItem) }
        return FolderEnumerator(ext: self, core: core, dirId: id)
    }
}
