import Darwin
import FileProvider
import IOKit.ps
import os

private let log = Logger(subsystem: "dev.vogel.homeport", category: "prefetch")

/// What the link allows right now.
struct PrefetchPolicy: Equatable {
    /// One level of children of the folder Finder shows (allowed on cellular too).
    var lookAhead = false
    /// Small files in the folder Finder shows, and the background crawl.
    var bulk = false
}

/// Materializes folders (and small files) before Finder asks for them.
///
/// The first access to a dataless item costs a fixed ~600 ms in macOS 26's
/// fileproviderd on top of the extension's own work (it holds the file
/// coordination claim that long after materializing). Anything materialized
/// ahead of time opens straight from disk, so this works from the user
/// visible tree: it reads materialized folders locally, finds dataless
/// children by `SF_DATALESS`, and asks the system to download them with
/// bounded concurrency, watching the flag clear to know when each is done.
actor Prefetcher {
    struct Stats {
        var folders = 0
        var files = 0
        var fileBytes = 0
        var failures = 0
    }

    private enum Job {
        case folder(URL, depth: Int)
        case file(URL, size: Int)

        var url: URL {
            switch self {
            case .folder(let u, _), .file(let u, _): return u
            }
        }
    }

    // Tunables.
    private let concurrency = 4
    /// The crawl leaves room for look-ahead, and is gentler on battery
    /// (fileproviderd does most of the work: ~5 ms of CPU per new item).
    private var crawlConcurrency: Int {
        IOPSGetTimeRemainingEstimate() == kIOPSTimeRemainingUnlimited ? 3 : 2
    }
    /// Root is 0, `dev` 1, a repo 2: the crawl materializes folders down to
    /// this depth, so a repo's own folders and their children are on disk.
    private let crawlDepth = 4
    private let smallFile = 256 * 1024
    private let folderFileBudget = 8 << 20
    private let crawlFileBudget = 1 << 20
    private let sessionFileBudget = 512 << 20
    private let lookAheadLimit = 48

    private static let skip: Set<String> = [
        "node_modules", ".git", "target", "build", "dist", ".next", "DerivedData", ".venv",
        "__pycache__", "Pods", ".build", ".turbo", ".cache", "vendor",
    ]
    /// Folder names people open first in a repo.
    private static let folderPrior: [String: Int] = [
        "src": 9, "Sources": 9, "app": 8, "apps": 8, "packages": 8, "crates": 8, "lib": 7,
        "docs": 7, "scripts": 6, "components": 6, "pages": 6, "server": 6, "client": 6,
        "web": 6, "cmd": 5, "internal": 5, "pkg": 5, "test": 4, "tests": 4, "apple": 5,
        "ios": 5, "macos": 5, "public": 3, "assets": 3, "examples": 3,
    ]
    /// Files people open first.
    private static let filePrior: [String: Int] = [
        "README.md": 9, "AGENTS.md": 8, "CLAUDE.md": 8, "package.json": 7, "Cargo.toml": 7,
        "go.mod": 6, "pyproject.toml": 6, "Makefile": 5, "Package.swift": 6, "index.ts": 5,
        "main.rs": 5, "lib.rs": 5, "main.go": 5, "index.html": 4, "TODO.md": 5,
    ]

    private let manager: NSFileProviderManager
    private var root: URL?
    private var policy = PrefetchPolicy()
    private var high: [Job] = []
    private var low: [Job] = []
    /// Materialized folders the crawl still has to read, breadth first.
    private var scan: [(URL, Int)] = []
    private var queued: Set<String> = []
    private var running = 0
    private var crawlStarted = false
    private var focus: String?
    private var stats = Stats()
    private var crawlStart: Date?

    init(manager: NSFileProviderManager) {
        self.manager = manager
    }

    func setRoot(_ url: URL) {
        root = url.standardizedFileURL
    }

    func setPolicy(_ p: PrefetchPolicy) {
        guard p != policy else { return }
        log.notice("policy look-ahead=\(p.lookAhead) bulk=\(p.bulk)")
        policy = p
        if p.bulk && !crawlStarted, let root {
            crawlStarted = true
            crawlStart = Date()
            scan.append((root, 0))
        }
        pump()
    }

    func currentStats() -> Stats { stats }

    /// Finder's front window shows `url`; `selected` are its selected items.
    func focus(on url: URL, selected: [URL]) {
        guard let root, url.standardizedFileURL.path.hasPrefix(root.path) else { return }
        let key = url.standardizedFileURL.path + "|" + selected.map(\.path).joined(separator: ",")
        guard key != focus else { return }
        focus = key
        // Newest focus first; older look-ahead that has not started is dropped.
        for j in high { queued.remove(j.url.path) }
        high.removeAll()
        let depth = url.standardizedFileURL.pathComponents.count - root.pathComponents.count
        var jobs: [Job] = []
        // Selected folders: their children are the likeliest next click.
        for s in selected where isMaterializedDir(s) {
            jobs += lookAhead(in: s, depth: depth + 1).prefix(16)
        }
        for s in selected where isDatalessDir(s) {
            jobs.append(.folder(s, depth: depth + 1))
        }
        jobs += lookAhead(in: url, depth: depth)
        jobs += smallFiles(in: url)
        for j in jobs where !queued.contains(j.url.path) {
            queued.insert(j.url.path)
            high.append(j)
        }
        if !jobs.isEmpty {
            log.notice("focus: \(jobs.count) look-ahead jobs")
        }
        pump()
    }

    // MARK: Finding work

    private struct Entry {
        let url: URL
        let name: String
        let isDir: Bool
        let dataless: Bool
        let size: Int
    }

    /// Children of a materialized folder (never triggers materialization).
    private func entries(of dir: URL) -> [Entry] {
        guard !lstatFlags(dir).dataless, let names = try? FileManager.default.contentsOfDirectory(atPath: dir.path) else {
            return []
        }
        return names.compactMap { name in
            guard !name.hasPrefix(".") else { return nil }
            let u = dir.appendingPathComponent(name)
            var st = stat()
            guard lstat(u.path, &st) == 0 else { return nil }
            let type = st.st_mode & S_IFMT
            guard type == S_IFDIR || type == S_IFREG else { return nil }
            return Entry(url: u, name: name, isDir: type == S_IFDIR,
                         dataless: st.st_flags & UInt32(SF_DATALESS) != 0, size: Int(st.st_size))
        }
    }

    private func lookAhead(in dir: URL, depth: Int) -> [Job] {
        guard policy.lookAhead else { return [] }
        let dirs = entries(of: dir).filter { $0.isDir && $0.dataless && !Self.skip.contains($0.name) }
        return dirs.sorted { a, b in
            let pa = Self.folderPrior[a.name] ?? 0, pb = Self.folderPrior[b.name] ?? 0
            return pa != pb ? pa > pb : a.name.localizedStandardCompare(b.name) == .orderedAscending
        }
        .prefix(lookAheadLimit).map { .folder($0.url, depth: depth + 1) }
    }

    private func smallFiles(in dir: URL, budget folderBudget: Int? = nil) -> [Job] {
        guard policy.bulk, stats.fileBytes < sessionFileBudget else { return [] }
        let files = entries(of: dir).filter { !$0.isDir && $0.dataless && $0.size > 0 && $0.size < smallFile }
        var budget = folderBudget ?? folderFileBudget
        var out: [Job] = []
        for f in files.sorted(by: { a, b in
            let pa = Self.filePrior[a.name] ?? 0, pb = Self.filePrior[b.name] ?? 0
            return pa != pb ? pa > pb : a.size < b.size
        }) where f.size <= budget {
            budget -= f.size
            out.append(.file(f.url, size: f.size))
        }
        return out
    }

    /// Reads materialized folders breadth first until it finds dataless
    /// folders to download (or runs out of tree).
    private func refillCrawl() {
        guard policy.bulk else { return }
        while low.count < concurrency * 4, !scan.isEmpty {
            let (dir, depth) = scan.removeFirst()
            // A repo's top-level files (README, manifests) are the likeliest
            // first opens: download the small ones too.
            if depth == 2 {
                for j in smallFiles(in: dir, budget: crawlFileBudget) where !queued.contains(j.url.path) {
                    queued.insert(j.url.path)
                    low.append(j)
                }
            }
            for e in entries(of: dir) where e.isDir && !Self.skip.contains(e.name) {
                if !e.dataless {
                    if depth + 1 < crawlDepth { scan.append((e.url, depth + 1)) }
                } else if !queued.contains(e.url.path) {
                    queued.insert(e.url.path)
                    low.append(.folder(e.url, depth: depth + 1))
                }
            }
        }
        if low.isEmpty && scan.isEmpty && crawlStarted, let start = crawlStart {
            crawlStart = nil
            let secs = Date().timeIntervalSince(start)
            log.notice("crawl done: \(self.stats.folders) folders in \(Int(secs)) s (\(String(format: "%.1f", Double(self.stats.folders) / max(secs, 1)), privacy: .public) /s), \(self.stats.failures) failures")
        }
    }

    // MARK: Running

    private func pump() {
        guard policy.lookAhead else { return }
        while running < concurrency {
            if high.isEmpty && low.isEmpty { refillCrawl() }
            let job: Job
            if !high.isEmpty {
                job = high.removeFirst()
            } else if policy.bulk, !low.isEmpty, running < crawlConcurrency {
                job = low.removeFirst()
            } else {
                return
            }
            if case .file = job, !policy.bulk {
                queued.remove(job.url.path)
                continue
            }
            running += 1
            Task { await self.run(job) }
        }
    }

    private func run(_ job: Job) async {
        let started = Date()
        let url = job.url
        do {
            if lstatFlags(url).dataless {
                let id: NSFileProviderItemIdentifier = try await withCheckedThrowingContinuation { c in
                    NSFileProviderManager.getIdentifierForUserVisibleFile(at: url) { id, _, error in
                        if let id { c.resume(returning: id) } else { c.resume(throwing: error ?? CocoaError(.fileNoSuchFile)) }
                    }
                }
                try await manager.requestDownloadForItem(withIdentifier: id)
                // The completion only acknowledges the request; done means
                // the dataless flag cleared.
                let deadline = Date().addingTimeInterval(30)
                while lstatFlags(url).dataless {
                    guard Date() < deadline else { throw CocoaError(.fileReadUnknown) }
                    try await Task.sleep(nanoseconds: 20_000_000)
                }
            }
            let ms = Int(Date().timeIntervalSince(started) * 1000)
            switch job {
            case .folder(let u, let depth):
                stats.folders += 1
                log.debug("materialized folder in \(ms) ms (depth \(depth))")
                if policy.bulk && depth < crawlDepth { scan.append((u, depth)) }
            case .file(_, let size):
                stats.files += 1
                stats.fileBytes += size
                log.debug("materialized \(size) byte file in \(ms) ms")
            }
        } catch {
            stats.failures += 1
            log.error("prefetch failed: \(String(describing: error), privacy: .public)")
        }
        queued.remove(url.path)
        running -= 1
        if (stats.folders + stats.files) % 100 == 0, stats.folders + stats.files > 0 {
            log.notice("prefetched \(self.stats.folders) folders, \(self.stats.files) files (\(self.stats.fileBytes) bytes), \(self.stats.failures) failures")
        }
        pump()
    }

    private func isMaterializedDir(_ u: URL) -> Bool {
        let f = lstatFlags(u)
        return f.isDir && !f.dataless
    }

    private func isDatalessDir(_ u: URL) -> Bool {
        let f = lstatFlags(u)
        return f.isDir && f.dataless && !Self.skip.contains(u.lastPathComponent)
    }
}

private func lstatFlags(_ u: URL) -> (dataless: Bool, isDir: Bool) {
    var st = stat()
    guard lstat(u.path, &st) == 0 else { return (false, false) }
    return (st.st_flags & UInt32(SF_DATALESS) != 0, st.st_mode & S_IFMT == S_IFDIR)
}
