import AppKit
import FileProvider
import Network
import SwiftUI
import os

private let log = Logger(subsystem: "dev.vogel.homeport", category: "app")

private final class ChangeForwarder: ChangeListener, @unchecked Sendable {
    let onChange: () -> Void
    init(_ f: @escaping () -> Void) { onChange = f }
    func changed() { onChange() }
}

/// One paired server: its File Provider domain, link monitor and prefetcher.
@MainActor
final class ServerController: ObservableObject, Identifiable {
    @Published private(set) var entry: ServerEntry
    /// Published only when it changes, so the menu is not redrawn every tick.
    @Published private(set) var linkState: LinkClass?
    @Published private(set) var status = "Connecting…"
    @Published private(set) var domainError: String?

    nonisolated let id: String
    private(set) var domain: NSFileProviderDomain
    private var core: Core?
    private var subscription: Subscription?
    private var wasOffline: Bool?
    private var signalPending = false
    private(set) var prefetcher: Prefetcher?
    /// Called when the server reports new facts about itself.
    var onInfo: ((ServerEntry) -> Void)?

    var manager: NSFileProviderManager? { NSFileProviderManager(for: domain) }

    init(entry: ServerEntry) {
        self.entry = entry
        self.id = entry.id
        self.domain = Self.makeDomain(entry)
    }

    static func makeDomain(_ entry: ServerEntry) -> NSFileProviderDomain {
        let d = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(entry.id), displayName: entry.name)
        // Finder's Move to Trash becomes a move into the trash container,
        // which homeportd maps to the server's Trash (and Put Back out of it).
        d.supportsSyncingTrash = entry.caps.contains(.trash)
        return d
    }

    func start() {
        addDomain()
        do {
            core = try Core(config: CoreConfig(
                server: entry.address, fingerprint: entry.fingerprint, clientId: entry.clientId,
                cachePath: nil, requestDeadlineMs: 1000, workerThreads: 1))
        } catch {
            status = "Client failed: \(error)"
            return
        }
        subscription = core?.subscribe(listener: ChangeForwarder { [weak self] in
            Task { @MainActor in self?.signalWorkingSetSoon() }
        })
    }

    func stop() {
        subscription?.cancel()
        subscription = nil
        core = nil
        prefetcher = nil
    }

    /// Once the domain is added: the prefetcher works in its folder.
    private func startPrefetcher() {
        guard prefetcher == nil, core != nil, let manager else { return }
        let name = entry.name
        manager.getUserVisibleURL(for: .rootContainer) { url, error in
            Task { @MainActor in
                guard let url else {
                    log.error("\(name, privacy: .public): user visible URL: \(String(describing: error), privacy: .public)")
                    return
                }
                guard self.prefetcher == nil, self.core != nil else { return }
                let p = Prefetcher(manager: manager)
                self.prefetcher = p
                await p.setRoot(url)
            }
        }
    }

    /// Adds (or updates) the domain. A domain whose trash setting differs
    /// is re-added: the setting is fixed when a domain is added.
    func addDomain() {
        let domain = self.domain
        NSFileProviderManager.getDomainsWithCompletionHandler { domains, _ in
            if let old = domains.first(where: { $0.identifier == domain.identifier }),
               old.supportsSyncingTrash != domain.supportsSyncingTrash {
                log.notice("re-adding \(domain.identifier.rawValue, privacy: .public) to change trash syncing")
                NSFileProviderManager.remove(old, mode: .preserveDirtyUserData) { _, error in
                    if let error { log.error("remove domain: \(String(describing: error), privacy: .public)") }
                    Task { @MainActor in self.add(domain) }
                }
                return
            }
            Task { @MainActor in self.add(domain) }
        }
    }

    private func add(_ domain: NSFileProviderDomain) {
        NSFileProviderManager.add(domain) { error in
            Task { @MainActor in
                if let error {
                    log.error("add domain \(domain.identifier.rawValue, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                    self.domainError = error.localizedDescription
                } else {
                    self.domainError = nil
                    self.startPrefetcher()
                }
            }
        }
    }

    func rename(to name: String) {
        entry.name = name
        domain = Self.makeDomain(entry)
        add(domain)
    }

    func tick(pathCheap: Bool) {
        guard let core else { return }
        let l = core.link()
        let state = l.probed ? l.state : nil
        if linkState != state { linkState = state }
        let text = Self.describe(l)
        if status != text { status = text }
        if let info = core.serverInfo().map(StoredServerInfo.init), info != entry.info {
            entry.info = info
            onInfo?(entry)
        }
        let policy = PrefetchPolicy(
            lookAhead: l.probed && l.state != .offline,
            bulk: l.probed && l.state == .good && pathCheap)
        if let prefetcher { Task { await prefetcher.setPolicy(policy) } }
        guard l.probed else { return }
        let offline = l.state == .offline
        defer { wasOffline = offline }
        guard let was = wasOffline, was != offline else { return }
        log.notice("\(self.entry.name, privacy: .public) is now \(offline ? "offline" : "online", privacy: .public)")
        if !offline {
            manager?.signalErrorResolved(NSFileProviderError(.serverUnreachable)) { _ in }
        }
        // Either way the root's decoration changes.
        signalWorkingSetSoon()
    }

    /// Coalesces bursts of change notifications.
    private func signalWorkingSetSoon() {
        guard !signalPending else { return }
        signalPending = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            guard let self else { return }
            self.signalPending = false
            self.manager?.signalEnumerator(for: .workingSet) { error in
                if let error { log.error("signal working set: \(String(describing: error), privacy: .public)") }
            }
        }
    }

    static func describe(_ l: LinkInfo) -> String {
        guard l.probed else { return "Connecting…" }
        let state: String
        switch l.state {
        case .good: state = "Good"
        case .degraded: state = "Degraded"
        case .poor: state = "Poor"
        case .offline: return "Offline"
        }
        if let rtt = l.pingMs ?? l.rttMs {
            // Coarse steps, so jitter does not republish (and redraw) the
            // status every second.
            if rtt < 10 { return "\(state) · < 10 ms" }
            return String(format: "%@ · %.0f ms", state, (rtt / 10).rounded() * 10)
        }
        return state
    }

    /// "macOS · homeportd 0.6.0 · 100.84.220.87:47820"
    var details: String {
        var parts: [String] = []
        if let i = entry.info { parts += [i.os, "homeportd \(i.version)"] }
        parts.append(entry.address)
        return parts.joined(separator: " · ")
    }

    func revealInFinder() {
        manager?.getUserVisibleURL(for: .rootContainer) { url, error in
            Task { @MainActor in
                if let url {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                } else {
                    log.error("user visible URL: \(String(describing: error), privacy: .public)")
                }
            }
        }
    }

    /// Removes the domain (its replica) and the cache, then adds it again.
    func resetCache() {
        status = "Resetting…"
        let entry = self.entry
        NSFileProviderManager.remove(domain, mode: .removeAll) { _, error in
            Task { @MainActor in
                if let error { log.error("remove domain: \(String(describing: error), privacy: .public)") }
                Self.deleteCache(of: entry)
                self.addDomain()
            }
        }
    }

    static func deleteCache(of entry: ServerEntry) {
        guard let db = ServerStore.cacheURL(for: entry) else { return }
        let fm = FileManager.default
        if entry.cachePath == ServerStore.legacyCachePath {
            for suffix in ["", "-wal", "-shm"] {
                try? fm.removeItem(at: URL(fileURLWithPath: db.path + suffix))
            }
        } else {
            try? fm.removeItem(at: db.deletingLastPathComponent())
        }
    }
}

/// Whether macOS lets Homeport's File Provider extension run.
enum ExtensionState: Equatable {
    case enabled
    case disabled
    /// Never chosen in System Settings (newly installed).
    case undecided
    case unknown
}

@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var servers: [ServerController] = []
    @Published private(set) var extensionState: ExtensionState = .unknown
    /// The worst link among the servers, for the menu bar icon.
    @Published private(set) var worstLink: LinkClass?
    /// A pairing link to show in the Add Server window.
    @Published var pendingLink: String?

    private var timer: Timer?
    private var finder: FinderWatcher?
    private let pathMonitor = NWPathMonitor()
    private var pathCheap = false
    private var ticks = 0
    let windows = WindowPresenter()

    init() {
        let list = ServerStore.load()
        if let url = ServerStore.listURL, !FileManager.default.fileExists(atPath: url.path), let first = list.first {
            // First run after 0.5: its config.json server becomes the first
            // entry, keeping its domain and cache (nothing re-downloads).
            log.notice("migrating config.json to servers.json (domain \(first.id, privacy: .public))")
            try? ServerStore.save(list)
        }
        for entry in list { attach(ServerController(entry: entry)) }
        pathMonitor.pathUpdateHandler = { [weak self] path in
            let cheap = path.status == .satisfied && !path.isExpensive && !path.isConstrained
            Task { @MainActor in self?.pathCheap = cheap }
        }
        pathMonitor.start(queue: .main)
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
        startFinderWatcher()
        Task {
            await refreshExtensionState()
            if needsOnboarding { showOnboarding() }
        }
    }

    var needsOnboarding: Bool {
        servers.isEmpty || extensionState == .disabled
    }

    private func attach(_ s: ServerController) {
        s.onInfo = { [weak self] _ in self?.save() }
        servers.append(s)
        s.start()
    }

    private func save() {
        do {
            try ServerStore.save(servers.map(\.entry))
        } catch {
            log.error("save servers.json: \(String(describing: error), privacy: .public)")
        }
    }

    private func startFinderWatcher() {
        guard finder == nil, !servers.isEmpty else { return }
        let watcher = FinderWatcher { [weak self] folder, selected in
            Task { @MainActor in
                for s in self?.servers ?? [] {
                    if let p = s.prefetcher { Task { await p.focus(on: folder, selected: selected) } }
                }
            }
        }
        watcher.start()
        finder = watcher
    }

    private func tick() {
        for s in servers { s.tick(pathCheap: pathCheap) }
        let states = servers.compactMap(\.linkState)
        let worst: LinkClass? = states.isEmpty ? nil : states.max { rank($0) < rank($1) }
        if worst != worstLink { worstLink = worst }
        ticks += 1
        if ticks % 5 == 0 { Task { await refreshExtensionState() } }
    }

    private func rank(_ l: LinkClass) -> Int {
        switch l {
        case .good: return 0
        case .degraded: return 1
        case .poor: return 2
        case .offline: return 3
        }
    }

    // MARK: Servers

    func server(withFingerprint fp: String) -> ServerController? {
        servers.first { $0.entry.fingerprint.lowercased() == fp.lowercased() }
    }

    /// Pins and adds a server the person confirmed.
    func add(probe: ProbeInfo, name: String) {
        if let existing = server(withFingerprint: probe.fingerprint) {
            log.notice("\(existing.entry.name, privacy: .public) is already added")
            return
        }
        var id = ServerEntry.domainId(fingerprint: probe.fingerprint)
        while servers.contains(where: { $0.id == id }) { id += "-2" }
        let entry = ServerEntry(
            id: id, name: name, address: probe.address, fingerprint: probe.fingerprint,
            clientId: "fp-" + UUID().uuidString.lowercased(), cachePath: "Cache/\(id)/metadata.sqlite",
            info: StoredServerInfo(probe.info))
        log.notice("adding \(name, privacy: .public) at \(probe.address, privacy: .public) as \(id, privacy: .public)")
        // Saved before the domain is added: its extension reads servers.json.
        let s = ServerController(entry: entry)
        s.onInfo = { [weak self] _ in self?.save() }
        servers.append(s)
        save()
        s.start()
        startFinderWatcher()
    }

    func rename(_ s: ServerController) {
        let alert = NSAlert()
        alert.messageText = "Rename \(s.entry.name)"
        alert.informativeText = "The name shown in Finder's sidebar."
        let field = NSTextField(string: s.entry.name)
        field.frame = NSRect(x: 0, y: 0, width: 240, height: 24)
        alert.accessoryView = field
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        NSApp.activate()
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let name = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty, name != s.entry.name else { return }
        s.rename(to: name)
        save()
    }

    func confirmRemove(_ s: ServerController) {
        let alert = NSAlert()
        alert.messageText = "Remove \(s.entry.name)?"
        alert.informativeText = "It leaves Finder's sidebar and its downloaded files are removed from this Mac. Nothing changes on the server. You can add it again later."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.first?.hasDestructiveAction = true
        NSApp.activate()
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        remove(s)
    }

    func remove(_ s: ServerController) {
        let entry = s.entry
        let manager = s.manager
        let online = s.linkState.map { $0 != .offline } ?? false
        s.stop()
        servers.removeAll { $0.id == s.id }
        save()
        guard online, let manager else {
            removeDomain(s.domain, entry: entry, mode: .preserveDirtyUserData)
            return
        }
        // Let edits made here reach the server first, then drop everything:
        // keeping "dirty" data keeps every downloaded file as a folder in
        // ~/Library/CloudStorage. If that does not finish soon (or fails),
        // unsynced edits are kept after all.
        var done = false
        let finish: @MainActor (Error?) -> Void = { error in
            guard !done else { return }
            done = true
            if let error { log.notice("\(entry.name, privacy: .public): pending changes not synced (\(String(describing: error), privacy: .public))") }
            self.removeDomain(s.domain, entry: entry, mode: error == nil ? .removeAll : .preserveDirtyUserData)
        }
        manager.waitForChanges(below: .rootContainer) { error in
            // The manager must live until the answer (its connection ends with it).
            withExtendedLifetime(manager) {}
            Task { @MainActor in finish(error) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 15) {
            MainActor.assumeIsolated { finish(CocoaError(.userCancelled)) }
        }
    }

    private func removeDomain(_ domain: NSFileProviderDomain, entry: ServerEntry, mode: NSFileProviderManager.DomainRemovalMode) {
        NSFileProviderManager.remove(domain, mode: mode) { preserved, error in
            Task { @MainActor in
                if let error {
                    log.error("remove domain \(entry.id, privacy: .public): \(String(describing: error), privacy: .public)")
                }
                ServerController.deleteCache(of: entry)
                if let preserved {
                    // Edits that had not reached the server yet.
                    log.notice("kept unsynced files of \(entry.name, privacy: .public) at \(preserved.path, privacy: .public)")
                    NSWorkspace.shared.activateFileViewerSelecting([preserved])
                }
            }
        }
    }

    // MARK: Windows and links

    func showAddServer(link: String? = nil) {
        if let link { pendingLink = link }
        windows.show("add-server", title: "Add Server") { AddServerView(model: self) }
    }

    func showOnboarding() {
        windows.show("onboarding", title: "Welcome to Homeport") { OnboardingView(model: self) }
    }

    /// `homeport://pair?…` adds a server; `homeport://add` and
    /// `homeport://setup` open Add Server and the setup guide.
    func open(_ url: URL) {
        guard url.scheme?.lowercased() == "homeport" else { return }
        switch url.host?.lowercased() {
        case "add": showAddServer(); return
        case "setup": showOnboarding(); return
        default: break
        }
        if servers.isEmpty || extensionState == .disabled {
            pendingLink = url.absoluteString
            showOnboarding()
        } else {
            showAddServer(link: url.absoluteString)
        }
    }

    // MARK: Extension state

    /// Reads the extension's election with pluginkit ("+" on, "-" off).
    func refreshExtensionState() async {
        let state = await Task.detached { () -> ExtensionState in
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/pluginkit")
            p.arguments = ["-m", "-i", "dev.vogel.homeport.HomeportFP"]
            let out = Pipe()
            p.standardOutput = out
            p.standardError = Pipe()
            do { try p.run() } catch { return .unknown }
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            let line = String(decoding: data, as: UTF8.self)
                .split(separator: "\n").first { $0.contains("dev.vogel.homeport.HomeportFP") }
            guard let line, let first = line.first else { return .unknown }
            switch first {
            case "+": return .enabled
            case "-": return .disabled
            default: return .undecided
            }
        }.value
        if state != extensionState {
            log.notice("File Provider extension: \(String(describing: state), privacy: .public)")
            extensionState = state
            if state == .enabled {
                // Domains added while it was off come up now.
                for s in servers { s.addDomain() }
            }
        }
    }

    func openExtensionSettings() {
        let urls = [
            "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.fileprovider-nonui",
            "x-apple.systempreferences:com.apple.LoginItems-Settings.extension",
        ]
        for s in urls {
            if let u = URL(string: s), NSWorkspace.shared.open(u) { return }
        }
    }
}

/// Plain AppKit windows for SwiftUI views (a menu bar app has no main window).
@MainActor
final class WindowPresenter {
    private var windows: [String: NSWindow] = [:]
    private var observers: [String: NSObjectProtocol] = [:]

    func show<V: View>(_ id: String, title: String, @ViewBuilder content: () -> V) {
        if let w = windows[id] {
            NSApp.activate()
            w.makeKeyAndOrderFront(nil)
            return
        }
        // The views have a fixed size; the window takes it once (letting
        // it follow content changes loops AppKit's constraint pass).
        let host = NSHostingController(rootView: content())
        host.sizingOptions = []
        let w = NSWindow(contentViewController: host)
        w.setContentSize(host.view.fittingSize)
        w.title = title
        w.styleMask = [.titled, .closable]
        w.isReleasedWhenClosed = false
        w.center()
        windows[id] = w
        observers[id] = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: w, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.forget(id) }
        }
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func close(_ id: String) {
        windows[id]?.close()
    }

    func isOpen(_ id: String) -> Bool { windows[id] != nil }

    private func forget(_ id: String) {
        windows[id] = nil
        if let o = observers.removeValue(forKey: id) { NotificationCenter.default.removeObserver(o) }
    }
}
