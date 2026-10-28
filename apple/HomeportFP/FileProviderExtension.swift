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
    case .Collision(let id):
        // Callers turn this into fileProviderErrorForCollision(with:).
        return NSFileProviderError(.filenameCollision, userInfo: [NSLocalizedDescriptionKey: "name is taken by \(id)"]) as NSError
    case .NotEmpty:
        return NSFileProviderError(.directoryNotEmpty) as NSError
    case .NoSpace:
        return NSFileProviderError(.insufficientQuota) as NSError
    case .Rejected(let msg):
        let underlying = NSError(domain: "dev.vogel.homeport", code: 1, userInfo: [NSLocalizedDescriptionKey: msg])
        return NSFileProviderError(.cannotSynchronize, userInfo: [NSUnderlyingErrorKey: underlying]) as NSError
    case .CloudOnly(let msg):
        return NSError(domain: NSCocoaErrorDomain, code: NSFileReadUnknownError, userInfo: [
            NSLocalizedDescriptionKey: "This file is only in the cloud on the server, so the server can't send it.",
            NSLocalizedRecoverySuggestionErrorKey: "Download it on the server first (for example \"Always Keep on This Device\" or \"Always keep on this device\" in OneDrive), then open it again.",
            NSDebugDescriptionErrorKey: msg,
        ])
    case .Denied(let msg):
        let underlying = NSError(domain: "dev.vogel.homeport", code: 2, userInfo: [NSLocalizedDescriptionKey: msg])
        return NSFileProviderError(.notAuthenticated, userInfo: [NSUnderlyingErrorKey: underlying]) as NSError
    }
}

/// Errors of write calls: transient server and I/O failures are retried by the system.
func fpWriteError(_ error: Error) -> NSError {
    if let e = error as? CoreError {
        switch e {
        case .Server(let msg), .Io(let msg):
            return NSError(domain: NSCocoaErrorDomain, code: NSFileWriteUnknownError,
                           userInfo: [NSLocalizedDescriptionKey: msg])
        default: break
        }
    }
    return fpError(error)
}

private func nanos(_ d: Date) -> Int64 { Int64((d.timeIntervalSince1970 * 1e9).rounded()) }

/// Big-endian content revision from an item version (nil before the first sync).
private func contentRev(_ v: NSFileProviderItemVersion) -> UInt64? {
    let d = v.contentVersion
    guard d.count == 8, d != NSFileProviderItemVersion.beforeFirstSyncComponent else { return nil }
    return d.reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
}

/// The metadata of `item` named by `fields`, for the server. `current` is
/// the item as the server has it (its mode is the base for permission
/// flags). Fields the server cannot keep (`caps`) are left out.
private func serverMeta(_ item: NSFileProviderItem, fields allFields: NSFileProviderItemFields, current: FfiItem?,
                        isDir: Bool, caps: ServerCaps) -> FfiMeta {
    var fields = allFields
    if !caps.contains(.tags) { fields.remove(.tagData) }
    if !caps.contains(.xattrs) { fields.remove(.extendedAttributes) }
    if !caps.contains(.setCrtime) { fields.remove(.creationDate) }
    var m = FfiMeta(mtimeNs: nil, crtimeNs: nil, mode: nil, hidden: nil, setTags: false, tags: nil, xattrs: [:])
    // A folder's modification time on the server tracks its own changes;
    // the local one (changed by every create here) is not pushed.
    if !isDir, fields.contains(.contentModificationDate), let d = item.contentModificationDate ?? nil { m.mtimeNs = nanos(d) }
    if fields.contains(.creationDate), let d = item.creationDate ?? nil { m.crtimeNs = nanos(d) }
    if fields.contains(.fileSystemFlags), let f = item.fileSystemFlags {
        var mode = current?.mode ?? (isDir ? 0o755 : 0o644)
        if !isDir {
            if f.contains(.userExecutable) {
                if mode & 0o100 == 0 { mode |= 0o100 | (mode & 0o044) >> 2 }
            } else {
                mode &= ~UInt32(0o111)
            }
        }
        if f.contains(.userWritable) { mode |= 0o200 } else { mode &= ~UInt32(0o222) }
        if mode != current?.mode && caps.contains(.mode) { m.mode = mode }
        let hidden = f.contains(.hidden)
        if hidden != ((current?.flags ?? 0) & ItemFlags.hidden != 0) && caps.contains(.hidden) { m.hidden = hidden }
    }
    if fields.contains(.tagData) {
        m.setTags = true
        m.tags = (item.tagData ?? nil).flatMap(TagCodec.xattr(fromTagData:))
    }
    if fields.contains(.extendedAttributes), let x = item.extendedAttributes {
        m.xattrs = x
    }
    return m
}

private func isEmpty(_ m: FfiMeta) -> Bool {
    m.mtimeNs == nil && m.crtimeNs == nil && m.mode == nil && m.hidden == nil && !m.setTags && m.xattrs.isEmpty
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
    /// The server this domain shows.
    let server: ServerEntry?

    private let pathMonitor = NWPathMonitor()
    /// The physical path is neither expensive (cellular, hotspot) nor
    /// constrained (Low Data Mode).
    private var pathCheap = false
    /// Folders whose small files were already requested in this process.
    private var siblingsRequested: Set<String> = []

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        self.manager = NSFileProviderManager(for: domain)
        let server = ServerStore.load().first { $0.id == domain.identifier.rawValue }
        var core: Core?
        if let server {
            do {
                core = try Core(config: CoreConfig(
                    server: server.address, fingerprint: server.fingerprint, clientId: server.clientId,
                    cachePath: ServerStore.cacheURL(for: server)?.path, requestDeadlineMs: 800, workerThreads: 2))
            } catch {
                log.error("core init failed: \(String(describing: error), privacy: .public)")
            }
        } else {
            log.error("no server for domain \(domain.identifier.rawValue, privacy: .public) in servers.json; start Homeport.app")
        }
        self.server = server
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
        log.notice("extension started for domain \(domain.identifier.rawValue, privacy: .public) (\(server?.address ?? "no server", privacy: .public))")
    }

    /// The server's name, for messages.
    var serverName: String { server?.name ?? domain.displayName }

    private var capsChecked: (caps: ServerCaps, at: Date)?

    /// What the server can do: from its latest Hello, else what the app
    /// last stored, else everything (as 0.5 assumed).
    var caps: ServerCaps {
        lock.lock()
        defer { lock.unlock() }
        if let c = capsChecked, Date().timeIntervalSince(c.at) < 5 { return c.caps }
        let caps = core?.serverInfo().map { ServerCaps(rawValue: $0.caps) } ?? server?.caps ?? .all
        capsChecked = (caps, Date())
        return caps
    }

    /// An item of this domain.
    func makeItem(_ raw: FfiItem, offline: Bool? = nil, echo: ItemEcho? = nil) -> Item {
        var raw = raw
        if raw.name.isEmpty {
            // The server names neither its root nor its trash container,
            // and the system rejects (and crashes on) nameless items.
            switch raw.id {
            case ItemID.serverRoot: raw.name = serverName
            case ItemID.serverTrash: raw.name = "Trash"
            default: raw.name = raw.id
            }
        }
        return Item(raw, offline: offline, echo: echo, caps: caps)
    }

    func rootItem(offline: Bool?) -> Item {
        Item.root(name: serverName, offline: offline)
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
            completionHandler(rootItem(offline: isOffline), nil)
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
                completionHandler(self.makeItem(raw, offline: shareRoot ? self.isOffline : nil), nil)
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
        let asked = Date()
        requestDownloads(ids)
        trailingKick(attempt: 0)
        // The cache only knows downloads it recorded; files downloaded
        // before that (or evicted) can look undownloaded and produce no
        // fetch. If no fetch has started shortly after, try files from
        // anywhere once more.
        guard kind != "anywhere for" else { return }
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.1) { [weak self] in
            guard let self else { return }
            self.lock.lock()
            let fetched = self.lastFetchStart.map { $0 >= asked } ?? false
            self.lock.unlock()
            guard !fetched else { return }
            let more = self.smallFiles(in: nil, excluding: opened, budget: 256 << 10, limit: 2)
            log.notice("no fetch after prefetching \(kind, privacy: .public) \(opened ?? dirId, privacy: .public); \(more.count) more from anywhere")
            self.requestDownloads(more)
        }
    }

    /// A reader can open one of the files this prefetch is fetching
    /// (Finder clicks right after a listing). That open waits on the
    /// prefetch's materialization, and with no fetch after it gets the
    /// whole hold (airbook: 685 ms, opened 3 ms after its prefetch fetch).
    /// So once the batch's fetches have gone quiet, one more small fetch.
    private func trailingKick(attempt: Int) {
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { [weak self] in
            guard let self else { return }
            self.lock.lock()
            let busy = self.lastFetchStart.map { Date().timeIntervalSince($0) < 0.1 } ?? false
            self.lock.unlock()
            if busy && attempt < 4 {
                self.trailingKick(attempt: attempt + 1)
                return
            }
            self.requestDownloads(self.smallFiles(in: nil, excluding: nil, budget: 64 << 10, limit: 1))
        }
    }

    private func requestDownloads(_ ids: [String]) {
        for id in ids {
            manager?.requestDownloadForItem(withIdentifier: NSFileProviderItemIdentifier(id)) { error in
                if let error { log.debug("nearby prefetch \(id, privacy: .public): \(String(describing: error), privacy: .public)") }
            }
        }
    }

    /// When the last fetch (of any kind) started.
    private var lastFetchStart: Date?

    private func noteFetchStart() {
        lock.lock()
        lastFetchStart = Date()
        lock.unlock()
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
        noteFetchStart()
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
                completionHandler(url, self.makeItem(raw), nil)
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
        noteFetchStart()
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
                   requestedVersion.contentVersion != self.makeItem(raw).itemVersion.contentVersion {
                    try? FileManager.default.removeItem(at: url)
                    throw NSFileProviderError(.versionNoLongerAvailable)
                }
                let range = len == 0 ? NSRange(location: 0, length: Int(done.size))
                                     : NSRange(location: Int(offset), length: Int(done.bytes))
                self.noteOutcome(nil)
                log.notice("fetched \(id, privacy: .public) range \(range.location)+\(range.length) in \(Int(Date().timeIntervalSince(started) * 1000)) ms for \(request.requestingExecutable?.lastPathComponent ?? "-", privacy: .public) system=\(request.isSystemRequest)")
                if len == 0 { self.noteFetched(id, contentRev: done.contentRev) }
                completionHandler(url, self.makeItem(raw), range, [], nil)
                if len == 0 { self.prefetchNearby(dirId: raw.parentId, opened: id, request: request) }
            } catch {
                self.noteOutcome(error)
                log.error("partial fetch \(id, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                completionHandler(nil, nil, NSRange(), [], fpError(error))
            }
        }
        return progress
    }

    /// File types the server's thumbnailer makes useful thumbnails for (as in
    /// homeportd's thumbs.rs); others keep their type icon without a request.
    private static let thumbnailExtensions: Set<String> = [
        "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "tif", "tiff", "bmp", "svg", "psd", "raw", "dng",
        "cr2", "nef", "arw", "mp4", "mov", "m4v", "mkv", "webm", "avi", "pdf", "key", "pages", "numbers", "usdz", "glb",
    ]

    /// Thumbnails come from the server, so Finder never downloads an image or
    /// video just to draw its icon.
    func fetchThumbnails(for itemIdentifiers: [NSFileProviderItemIdentifier], requestedSize size: CGSize,
                         perThumbnailCompletionHandler: @escaping (NSFileProviderItemIdentifier, Data?, Error?) -> Void,
                         completionHandler: @escaping (Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: Int64(itemIdentifiers.count))
        guard let core else {
            completionHandler(NSFileProviderError(.serverUnreachable))
            return progress
        }
        guard caps.contains(.thumbnails) else {
            // The server makes none: Finder keeps the type icons.
            for identifier in itemIdentifiers { perThumbnailCompletionHandler(identifier, nil, nil) }
            progress.completedUnitCount = progress.totalUnitCount
            completionHandler(nil)
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

    /// The item behind a collision, for fileProviderErrorForCollision(with:).
    private func collisionError(_ error: Error) -> NSError? {
        guard let e = error as? CoreError, case .Collision(let id) = e, let core,
              let existing = try? core.cachedItem(id: id) else { return nil }
        return NSError.fileProviderErrorForCollision(with: makeItem(existing))
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let name = itemTemplate.filename
        guard let core else {
            completionHandler(nil, [], false, NSFileProviderError(.serverUnreachable))
            return progress
        }
        // Junk (.DS_Store, editor temporaries), and anything outside the
        // shares (the root itself, items created right in the Trash) stays local.
        let parentId = ItemID.server(itemTemplate.parentItemIdentifier)
        if isJunkName(name: name) || parentId == nil || parentId == ItemID.serverRoot || parentId == ItemID.serverTrash {
            log.notice("create \(name, privacy: .public): excluded from sync")
            completionHandler(nil, [], false, NSFileProviderError(.excludedFromSync))
            return progress
        }
        let type = itemTemplate.contentType ?? .data
        var isDirURL: ObjCBool = false
        let urlIsDir = url.map { FileManager.default.fileExists(atPath: $0.path, isDirectory: &isDirURL) && isDirURL.boolValue } ?? false
        let kind: ItemKind
        if type.conforms(to: .symbolicLink) {
            kind = .symlink
        } else if type.conforms(to: .folder) || type.conforms(to: .directory) || urlIsDir {
            kind = .dir
        } else {
            kind = .file
        }
        if kind == .symlink && !caps.contains(.symlinks) {
            log.notice("create \(name, privacy: .public): \(self.serverName, privacy: .public) cannot make symlinks")
            let underlying = NSError(domain: "dev.vogel.homeport", code: 3, userInfo: [
                NSLocalizedDescriptionKey: "\(serverName) can't store symbolic links.",
            ])
            completionHandler(nil, [], false, NSFileProviderError(.cannotSynchronize, userInfo: [NSUnderlyingErrorKey: underlying]))
            return progress
        }
        let isPackage = kind == .dir && urlIsDir
        let size = (kind == .file ? url.flatMap { try? $0.resourceValues(forKeys: [.fileSizeKey]).fileSize } : nil) ?? 0
        progress.totalUnitCount = Int64(max(size, 1))
        let token = CancelToken()
        progress.cancellationHandler = { token.cancel() }
        let caps = self.caps
        let meta = serverMeta(itemTemplate, fields: fields, current: nil, isDir: kind == .dir, caps: caps)
        let opId = itemTemplate.itemIdentifier.rawValue
        let echo = ItemEcho(from: itemTemplate)
        let started = Date()
        Task {
            do {
                var raw = try await core.createItem(
                    opId: opId, parentId: parentId!, name: name, kind: kind,
                    contentPath: kind == .file ? url?.path : nil,
                    linkTarget: kind == .symlink ? (itemTemplate.symlinkTargetPath ?? nil) : nil,
                    meta: meta, mayExist: options.contains(.mayAlreadyExist),
                    progress: ProgressForwarder(progress), cancel: token)
                if isPackage, let url {
                    try await self.uploadTree(url, into: raw.id, core: core, token: token, caps: caps)
                    raw = try await core.item(id: raw.id)
                }
                progress.completedUnitCount = progress.totalUnitCount
                self.noteOutcome(nil)
                log.notice("created \(name, privacy: .public) (\(String(describing: kind), privacy: .public), \(size) bytes) as \(raw.id, privacy: .public) in \(Int(Date().timeIntervalSince(started) * 1000)) ms")
                completionHandler(self.makeItem(raw, echo: echo), [], false, nil)
            } catch {
                self.noteOutcome(error)
                if let c = self.collisionError(error) {
                    log.notice("create \(name, privacy: .public): name taken")
                    completionHandler(nil, [], false, c)
                    return
                }
                log.error("create \(name, privacy: .public) failed after \(Int(Date().timeIntervalSince(started) * 1000)) ms: \(String(describing: error), privacy: .public)")
                completionHandler(nil, [], false, fpWriteError(error))
            }
        }
        return progress
    }

    /// A package (an .app, .xcodeproj, .rtfd...) arrives as one item whose
    /// URL is a directory: its contents are uploaded here, item by item.
    private func uploadTree(_ dir: URL, into parentId: String, core: Core, token: CancelToken, caps: ServerCaps) async throws {
        let fm = FileManager.default
        for child in try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.isDirectoryKey, .isSymbolicLinkKey]) {
            let name = child.lastPathComponent
            if isJunkName(name: name) { continue }
            let v = try child.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey, .contentModificationDateKey])
            var meta = FfiMeta(mtimeNs: v.contentModificationDate.map(nanos), crtimeNs: nil, mode: nil, hidden: nil,
                               setTags: false, tags: nil, xattrs: [:])
            if caps.contains(.mode), let attrs = try? fm.attributesOfItem(atPath: child.path),
               let p = attrs[.posixPermissions] as? NSNumber {
                meta.mode = p.uint32Value
            }
            let op = "pkg-\(parentId)-\(name)"
            if v.isSymbolicLink == true {
                guard caps.contains(.symlinks) else { continue }
                let target = try fm.destinationOfSymbolicLink(atPath: child.path)
                _ = try? await core.createItem(opId: op, parentId: parentId, name: name, kind: .symlink, contentPath: nil,
                                               linkTarget: target, meta: meta, mayExist: true, progress: nil, cancel: token)
            } else if v.isDirectory == true {
                let d = try await core.createItem(opId: op, parentId: parentId, name: name, kind: .dir, contentPath: nil,
                                                  linkTarget: nil, meta: meta, mayExist: true, progress: nil, cancel: token)
                try await uploadTree(child, into: d.id, core: core, token: token, caps: caps)
            } else {
                _ = try await core.createItem(opId: op, parentId: parentId, name: name, kind: .file, contentPath: child.path,
                                              linkTarget: nil, meta: meta, mayExist: true, progress: nil, cancel: token)
            }
        }
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields,
                    contents newContents: URL?, options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let core, let id = ItemID.server(item.itemIdentifier) else {
            completionHandler(nil, [], false, NSFileProviderError(.noSuchItem))
            return progress
        }
        let echo = ItemEcho(from: item)
        let cached = try? core.cachedItem(id: id)
        if id == ItemID.serverRoot || ItemID.isShareRoot(id) {
            // Nothing about the root or the shares can change from here.
            let raw = cached ?? rootItem(offline: nil).raw
            completionHandler(makeItem(raw, echo: echo), [], false, nil)
            return progress
        }
        let isDir = cached?.kind == .dir || (item.contentType ?? .data).conforms(to: .folder)
        let inTrash = cached?.parentId == ItemID.serverTrash
        let toTrash = changedFields.contains(.parentItemIdentifier) && item.parentItemIdentifier == .trashContainer
        let move = changedFields.contains(.parentItemIdentifier) || changedFields.contains(.filename)
        let contents = changedFields.contains(.contents) && newContents != nil
        let caps = self.caps
        let meta = serverMeta(item, fields: changedFields, current: cached, isDir: isDir, caps: caps)
        let size = newContents.flatMap { try? $0.resourceValues(forKeys: [.fileSizeKey]).fileSize } ?? 0
        progress.totalUnitCount = Int64(max(size, 1))
        let token = CancelToken()
        progress.cancellationHandler = { token.cancel() }
        let started = Date()
        let what = "\(id) \(item.filename) fields=\(changedFields.rawValue)"
        Task {
            do {
                var current: FfiItem? = nil
                var fetch = false
                var metaDone = false
                if toTrash && !inTrash {
                    current = try await core.trashItem(id: id, listed: true, recursive: true)
                } else if move && inTrash && !toTrash {
                    guard let parent = ItemID.server(item.parentItemIdentifier) else { throw NSFileProviderError(.noSuchItem) }
                    current = try await core.untrashItem(id: id, parentId: parent, name: item.filename)
                } else if move && !inTrash {
                    let parent = changedFields.contains(.parentItemIdentifier) ? ItemID.server(item.parentItemIdentifier) : nil
                    let name = changedFields.contains(.filename) ? item.filename : nil
                    current = try await core.modifyItem(id: id, newParentId: parent, newName: name,
                                                        meta: contents ? serverMeta(item, fields: [], current: cached, isDir: isDir, caps: caps) : meta)
                    metaDone = !contents
                }
                if contents, let url = newContents, !inTrash && !toTrash {
                    let out = try await core.writeContent(id: id, baseContentRev: contentRev(version), contentPath: url.path,
                                                          meta: meta, progress: ProgressForwarder(progress), cancel: token)
                    current = out.item
                    metaDone = true
                    if let copy = out.conflict {
                        // The edit lives on as the copy; this item gets the server's content.
                        fetch = true
                        log.notice("conflict on \(item.filename, privacy: .public): kept the edit as \(copy.name, privacy: .public)")
                        self.signalWorkingSet()
                    }
                }
                if !metaDone && !isEmpty(meta) && !inTrash && !toTrash {
                    current = try await core.modifyItem(id: id, newParentId: nil, newName: nil, meta: meta)
                }
                let raw: FfiItem
                if let current { raw = current } else if let cached { raw = cached } else { raw = try await core.item(id: id) }
                progress.completedUnitCount = progress.totalUnitCount
                self.noteOutcome(nil)
                log.notice("modified \(what, privacy: .public) (\(size) bytes) in \(Int(Date().timeIntervalSince(started) * 1000)) ms\(fetch ? " (conflict)" : "", privacy: .public)")
                completionHandler(self.makeItem(raw, echo: echo), [], fetch, nil)
            } catch {
                self.noteOutcome(error)
                if let c = self.collisionError(error) {
                    log.notice("modify \(what, privacy: .public): name taken")
                    completionHandler(nil, [], false, c)
                    return
                }
                log.error("modify \(what, privacy: .public) failed after \(Int(Date().timeIntervalSince(started) * 1000)) ms: \(String(describing: error), privacy: .public)")
                completionHandler(nil, [], false, fpWriteError(error))
            }
        }
        return progress
    }

    /// Deletes never destroy anything on the server: an item deleted here
    /// moves to the server's Trash, and emptying this Mac's Trash only drops
    /// the items from the trash container (they stay in the server's Trash).
    /// A server without a Trash refuses deletes (its items do not offer them).
    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let core, let id = ItemID.server(identifier) else {
            completionHandler(nil)
            return progress
        }
        if id == ItemID.serverRoot || ItemID.isShareRoot(id) || id == ItemID.serverTrash {
            completionHandler(NSFileProviderError(.deletionRejected))
            return progress
        }
        let inTrash = (try? core.cachedItem(id: id))??.parentId == ItemID.serverTrash
        Task {
            do {
                if inTrash {
                    try await core.forgetItem(id: id)
                } else {
                    _ = try await core.trashItem(id: id, listed: false, recursive: options.contains(.recursive))
                }
                self.noteOutcome(nil)
                log.notice("deleted \(id, privacy: .public) (\(inTrash ? "left in the server's Trash" : "moved to the server's Trash", privacy: .public))")
                completionHandler(nil)
            } catch CoreError.NotFound {
                completionHandler(nil)
            } catch {
                self.noteOutcome(error)
                log.error("delete \(id, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                completionHandler(fpWriteError(error))
            }
        }
        return progress
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
