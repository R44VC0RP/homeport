import FileProvider

enum AnchorCodec {
    static func encode(_ a: Anchor) -> NSFileProviderSyncAnchor {
        var d = Data()
        withUnsafeBytes(of: a.indexId.bigEndian) { d.append(contentsOf: $0) }
        withUnsafeBytes(of: a.seq.bigEndian) { d.append(contentsOf: $0) }
        return NSFileProviderSyncAnchor(d)
    }

    static func decode(_ s: NSFileProviderSyncAnchor) -> Anchor? {
        let d = s.rawValue
        guard d.count == 16 else { return nil }
        let be = { (r: Range<Int>) in d.subdata(in: r).reduce(UInt64(0)) { $0 << 8 | UInt64($1) } }
        return Anchor(indexId: be(0..<8), seq: be(8..<16))
    }
}

private enum PageCodec {
    static func isInitial(_ p: NSFileProviderPage) -> Bool {
        p.rawValue == (NSFileProviderPage.initialPageSortedByName as NSFileProviderPage).rawValue
            || p.rawValue == (NSFileProviderPage.initialPageSortedByDate as NSFileProviderPage).rawValue
    }

    static func encode(_ offset: UInt32) -> NSFileProviderPage {
        NSFileProviderPage(withUnsafeBytes(of: offset.bigEndian) { Data($0) })
    }

    static func decode(_ p: NSFileProviderPage) -> UInt32 {
        p.rawValue.count == 4 ? p.rawValue.reduce(UInt32(0)) { $0 << 8 | UInt32($1) } : 0
    }
}

private func pageSize(_ observer: NSFileProviderEnumerationObserver) -> UInt32 {
    UInt32(min(max(observer.suggestedPageSize ?? 500, 100), 2000))
}

/// Lists one folder: from the server the first time, then from the local cache.
final class FolderEnumerator: NSObject, NSFileProviderEnumerator {
    let ext: FileProviderExtension
    let core: Core
    let dirId: String

    init(ext: FileProviderExtension, core: Core, dirId: String) {
        self.ext = ext
        self.core = core
        self.dirId = dirId
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let limit = pageSize(observer)
        let initial = PageCodec.isInitial(page)
        Task {
            let started = Date()
            do {
                var offset: UInt32 = 0
                if initial {
                    let st = try await core.list(dirId: dirId)
                    let ms = Int(Date().timeIntervalSince(started) * 1000)
                    log.notice("list \(self.dirId, privacy: .public): \(st.total) items, cache=\(st.fromCache) stale=\(st.stale) in \(ms) ms")
                    if !st.fromCache { self.ext.noteOutcome(nil) }
                    if self.core.hasPendingChanges() { self.ext.signalWorkingSet() }
                } else {
                    offset = PageCodec.decode(page)
                }
                let raw = try core.page(dirId: dirId, offset: offset, limit: limit)
                let shareRoots = dirId == ItemID.serverRoot
                let offline = shareRoots ? ext.isOffline : nil
                observer.didEnumerate(raw.map { Item($0, offline: offline) })
                observer.finishEnumerating(upTo: raw.count == Int(limit) ? PageCodec.encode(offset + limit) : nil)
            } catch {
                self.ext.noteOutcome(error)
                log.error("list \(self.dirId, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                observer.finishEnumeratingWithError(fpError(error))
            }
        }
    }

    // Replicated extensions report changes through the working set.
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(NSFileProviderSyncAnchor(Data([0])))
    }
}

/// Everything this client has listed; changes come from the server's change feed.
final class WorkingSetEnumerator: NSObject, NSFileProviderEnumerator {
    let ext: FileProviderExtension
    let core: Core

    init(ext: FileProviderExtension, core: Core) {
        self.ext = ext
        self.core = core
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let limit = pageSize(observer)
        let offset = PageCodec.isInitial(page) ? 0 : PageCodec.decode(page)
        do {
            let raw = try core.workingSetPage(offset: offset, limit: limit)
            let offline = ext.isOffline
            observer.didEnumerate(raw.map { Item($0, offline: $0.parentId == ItemID.serverRoot ? offline : nil) })
            observer.finishEnumerating(upTo: raw.count == Int(limit) ? PageCodec.encode(offset + limit) : nil)
        } catch {
            observer.finishEnumeratingWithError(fpError(error))
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        guard let since = AnchorCodec.decode(anchor) else {
            observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
            return
        }
        Task {
            do {
                let cs = try await core.changes(since: since)
                // The root items always come along: their version carries the
                // link state, so the "Disconnected" decoration clears.
                let changed = cs.changed.filter { $0.parentId != ItemID.serverRoot }
                let updates = changed.map { Item($0) } + rootItems(offline: false)
                for chunk in stride(from: 0, to: updates.count, by: 500) {
                    observer.didUpdate(Array(updates[chunk..<min(chunk + 500, updates.count)]))
                }
                if !cs.deleted.isEmpty {
                    observer.didDeleteItems(withIdentifiers: cs.deleted.map(ItemID.system))
                }
                ext.noteOutcome(nil)
                log.notice("changes since \(since.seq): \(cs.changed.count) changed, \(cs.deleted.count) deleted, now \(cs.anchor.seq)")
                observer.finishEnumeratingChanges(upTo: AnchorCodec.encode(cs.anchor), moreComing: false)
            } catch CoreError.Unreachable(let msg) {
                ext.noteOutcome(CoreError.Unreachable(msg: msg))
                log.error("changes: unreachable (\(msg, privacy: .public)); decorating root")
                observer.didUpdate(rootItems(offline: true))
                observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
            } catch {
                log.error("changes failed: \(String(describing: error), privacy: .public)")
                observer.finishEnumeratingWithError(fpError(error))
            }
        }
    }

    private func rootItems(offline: Bool) -> [Item] {
        let shares = (try? core.page(dirId: ItemID.serverRoot, offset: 0, limit: 64)) ?? []
        return [Item.root(offline: offline)] + shares.map { Item($0, offline: offline) }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        Task {
            do {
                completionHandler(AnchorCodec.encode(try await core.syncAnchor()))
            } catch {
                log.error("currentSyncAnchor failed: \(String(describing: error), privacy: .public)")
                completionHandler(nil)
            }
        }
    }
}
