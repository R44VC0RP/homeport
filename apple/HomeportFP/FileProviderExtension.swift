import FileProvider
import Network
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

final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderPartialContentFetching,
    NSFileProviderThumbnailing {
    let domain: NSFileProviderDomain
    let manager: NSFileProviderManager?
    let core: Core?

    private let pathMonitor = NWPathMonitor()
    /// The physical path is neither expensive (cellular, hotspot) nor
    /// constrained (Low Data Mode).
    private var pathCheap = false
    /// Folders whose small files were already requested in this process.
    private var siblingsRequested: Set<String> = []

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
        pathMonitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            let cheap = path.status == .satisfied && !path.isExpensive && !path.isConstrained
            self.lock.lock()
            self.pathCheap = cheap
            self.lock.unlock()
        }
        pathMonitor.start(queue: DispatchQueue(label: "dev.vogel.homeport.path"))
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

    /// Someone opened a file in `dirId`, or listed `dirId` for the first
    /// time. Two reasons to download small files nearby right away:
    ///
    /// - They are the likeliest next opens.
    /// - On macOS 26, fileproviderd holds the opener for a flat ~600 ms
    ///   after the item has materialized, and the hold ends early when
    ///   another item's content is fetched meanwhile (airbook: first opens
    ///   with siblings to fetch took 70-91 ms, lone files 362-716 ms;
    ///   signalling the working set or re-requesting the parent or the item
    ///   did not help). So when the folder has nothing left to fetch, one or
    ///   two small files from neighbouring folders are fetched instead.
    ///
    /// Good, unmetered links only; user requests only (fetches that
    /// prefetching itself caused are system requests).
    func prefetchNearby(dirId: String, opened: String?, request: NSFileProviderRequest) {
        if request.isSystemRequest || request.requestingExecutable?.path.contains("/Homeport.app/") == true { return }
        guard let core, let manager else { return }
        let link = core.link()
        guard link.probed, link.state == .good else { return }
        lock.lock()
        let cheap = pathCheap
        let firstTime = siblingsRequested.insert(dirId).inserted
        lock.unlock()
        guard cheap else { return }
        var ids = firstTime ? smallFiles(in: dirId, excluding: opened, budget: 4 << 20, limit: 48) : []
        var kind = "beside"
        if ids.isEmpty, let dir = try? core.cachedItem(id: dirId) {
            // Small files of the folder's own subfolders, then of its
            // neighbours (listings cached when the crawl or Finder got
            // there), then of its parent. The parent's often come first in
            // the crawl and are already on disk (no fetch, no effect: one
            // airbook miss), so it is the last resort, and three are asked
            // for in case some are already downloaded.
            kind = "near"
            let want = 3
            let children = ((try? core.page(dirId: dirId, offset: 0, limit: 2000)) ?? []).filter { $0.kind == .dir }
            let neighbours = ((try? core.page(dirId: dir.parentId, offset: 0, limit: 2000)) ?? [])
                .filter { $0.kind == .dir && $0.id != dirId }
            for n in (children.prefix(8) + neighbours.prefix(8)) where ids.count < want {
                ids += smallFiles(in: n.id, excluding: nil, budget: 256 << 10, limit: want - ids.count)
            }
            if ids.count < want {
                ids += smallFiles(in: dir.parentId, excluding: nil, budget: 256 << 10, limit: want - ids.count)
            }
        }
        if ids.isEmpty {
            // Nothing undownloaded nearby (or a folder without files): any
            // one real fetch ends the hold, so take a few small files from
            // anywhere in the cache.
            kind = "anywhere for"
            ids = smallFiles(in: nil, excluding: opened, budget: 256 << 10, limit: 2)
        }
        guard !ids.isEmpty else {
            log.notice("prefetching 0 small files for \(opened ?? dirId, privacy: .public) (no undownloaded candidates)")
            return
        }
        log.notice("prefetching \(ids.count) small files \(kind, privacy: .public) \(opened ?? dirId, privacy: .public)")
        for id in ids {
            manager.requestDownloadForItem(withIdentifier: NSFileProviderItemIdentifier(id)) { error in
                if let error { log.debug("nearby prefetch \(id, privacy: .public): \(String(describing: error), privacy: .public)") }
            }
        }
    }

    /// A user's first listing waiting for its folder to materialize.
    private var pendingListing: (dirId: String, request: NSFileProviderRequest, at: Date)?

    /// After a user's first listing, the folder's small files are fetched
    /// once fileproviderd reports the folder materialized: that is when its
    /// ~600 ms hold starts, and a fetch during the hold ends it. Fetching
    /// right after the listing is served finishes before the hold begins
    /// (airbook: no effect on first listings). Falls back to a timer.
    func listedForUser(dirId: String, request: NSFileProviderRequest) {
        if request.isSystemRequest || request.requestingExecutable?.path.contains("/Homeport.app/") == true { return }
        lock.lock()
        pendingListing = (dirId, request, Date())
        lock.unlock()
        DispatchQueue.global().asyncAfter(deadline: .now() + 1.5) { [weak self] in
            self?.firePendingListing(dirId: dirId, why: "timer")
        }
    }

    private func firePendingListing(dirId: String?, why: String) {
        lock.lock()
        guard let p = pendingListing, dirId == nil || p.dirId == dirId, Date().timeIntervalSince(p.at) < 5 else {
            lock.unlock()
            return
        }
        pendingListing = nil
        lock.unlock()
        log.notice("listing \(p.dirId, privacy: .public) materialized (\(why, privacy: .public), \(Int(Date().timeIntervalSince(p.at) * 1000)) ms after listing)")
        prefetchNearby(dirId: p.dirId, opened: nil, request: p.request)
    }

    func materializedItemsDidChange(completionHandler: @escaping () -> Void) {
        firePendingListing(dirId: nil, why: "materializedItemsDidChange")
        completionHandler()
    }

    /// Small files never downloaded (per the cache's record of fetches) and
    /// not yet requested by this process: in `dirId` smallest first, or
    /// (`nil`) anywhere at random. Marks them requested.
    private func smallFiles(in dirId: String?, excluding: String?, budget: UInt64, limit: Int) -> [String] {
        guard let core, let items = try? core.smallUnfetchedFiles(dirId: dirId, maxSize: 256 * 1024, limit: 200) else {
            return []
        }
        var budget = budget
        var out: [String] = []
        lock.lock()
        defer { lock.unlock() }
        for i in items where i.id != excluding && !filesRequested.contains(i.id) {
            guard out.count < limit else { break }
            guard i.size <= budget else { continue }
            budget -= i.size
            out.append(i.id)
        }
        filesRequested.formUnion(out)
        if filesRequested.count > 100_000 { filesRequested.removeAll() }
        return out
    }

    /// Files fetched or requested by this process (so prefetch does not
    /// ask for them again).
    private var filesRequested: Set<String> = []

    private func noteFetched(_ id: String, contentRev: UInt64) {
        lock.lock()
        filesRequested.insert(id)
        lock.unlock()
        try? core?.noteFetched(id: id, contentRev: contentRev)
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
                log.notice("fetched \(id, privacy: .public) \(done.bytes) bytes in \(Int(Date().timeIntervalSince(started) * 1000)) ms for \(request.requestingExecutable?.lastPathComponent ?? "-", privacy: .public) system=\(request.isSystemRequest)")
                self.noteFetched(id, contentRev: done.contentRev)
                completionHandler(url, Item(raw), nil)
                self.prefetchNearby(dirId: raw.parentId, opened: id, request: request)
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
                log.notice("fetched \(id, privacy: .public) range \(range.location)+\(range.length) in \(Int(Date().timeIntervalSince(started) * 1000)) ms for \(request.requestingExecutable?.lastPathComponent ?? "-", privacy: .public) system=\(request.isSystemRequest)")
                if len == 0 { self.noteFetched(id, contentRev: done.contentRev) }
                completionHandler(url, Item(raw), range, [], nil)
                if len == 0 { self.prefetchNearby(dirId: raw.parentId, opened: id, request: request) }
            } catch {
                self.noteOutcome(error)
                log.error("partial fetch \(id, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                completionHandler(nil, nil, NSRange(), [], fpError(error))
            }
        }
        return progress
    }

    /// File types basebook's Quick Look makes useful thumbnails for (as in
    /// homeportd's thumbs.rs); others keep their type icon without a request.
    private static let thumbnailExtensions: Set<String> = [
        "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "tif", "tiff", "bmp", "svg", "psd", "raw", "dng",
        "cr2", "nef", "arw", "mp4", "mov", "m4v", "mkv", "webm", "avi", "pdf", "key", "pages", "numbers", "usdz", "glb",
    ]

    /// Thumbnails come from basebook, so Finder never downloads an image or
    /// video just to draw its icon.
    func fetchThumbnails(for itemIdentifiers: [NSFileProviderItemIdentifier], requestedSize size: CGSize,
                         perThumbnailCompletionHandler: @escaping (NSFileProviderItemIdentifier, Data?, Error?) -> Void,
                         completionHandler: @escaping (Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: Int64(itemIdentifiers.count))
        guard let core else {
            completionHandler(NSFileProviderError(.serverUnreachable))
            return progress
        }
        // Icons rarely need more than 512 px, and smaller ones are made faster.
        let pixels = UInt32(max(64, min(512, max(size.width, size.height))))
        let task = Task {
            let started = Date()
            var made = 0
            await withTaskGroup(of: Bool.self) { group in
                var running = 0
                for identifier in itemIdentifiers {
                    if Task.isCancelled { break }
                    guard let id = ItemID.server(identifier),
                          let item = try? core.cachedItem(id: id), item.kind == .file,
                          Self.thumbnailExtensions.contains((item.name as NSString).pathExtension.lowercased())
                    else {
                        perThumbnailCompletionHandler(identifier, nil, nil)
                        progress.completedUnitCount += 1
                        continue
                    }
                    if running >= 4 {
                        if await group.next() == true { made += 1 }
                        running -= 1
                    }
                    running += 1
                    group.addTask {
                        defer { progress.completedUnitCount += 1 }
                        do {
                            let png = try await core.thumbnail(id: id, maxPixels: pixels)
                            perThumbnailCompletionHandler(identifier, png, nil)
                            return png != nil
                        } catch {
                            perThumbnailCompletionHandler(identifier, nil, fpError(error))
                            return false
                        }
                    }
                }
                for await ok in group where ok { made += 1 }
            }
            log.notice("thumbnails: \(made)/\(itemIdentifiers.count) at \(pixels) px in \(Int(Date().timeIntervalSince(started) * 1000)) ms")
            completionHandler(Task.isCancelled ? CocoaError(.userCancelled) : nil)
        }
        progress.cancellationHandler = { task.cancel() }
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
        return FolderEnumerator(ext: self, core: core, dirId: id, request: request)
    }
}
