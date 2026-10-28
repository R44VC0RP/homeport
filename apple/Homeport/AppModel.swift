import AppKit
import FileProvider
import os

private let log = Logger(subsystem: "dev.vogel.homeport", category: "app")

private final class ChangeForwarder: ChangeListener, @unchecked Sendable {
    let onChange: () -> Void
    init(_ f: @escaping () -> Void) { onChange = f }
    func changed() { onChange() }
}

@MainActor
final class AppModel: ObservableObject {
    /// What the menu bar icon shows; published only when it changes, so the
    /// status item is not redrawn on every tick.
    @Published var linkState: LinkClass?
    @Published var status = "Starting…"
    @Published var server = HomeportConfig.defaultServer
    @Published var domainError: String?

    private var core: Core?
    private var subscription: Subscription?
    private var timer: Timer?
    private var wasOffline: Bool?
    private var signalPending = false

    let domain: NSFileProviderDomain = {
        let d = NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(HomeportConfig.domainIdentifier),
            displayName: HomeportConfig.domainDisplayName)
        d.supportsSyncingTrash = false
        return d
    }()

    var manager: NSFileProviderManager? { NSFileProviderManager(for: domain) }

    init() {
        start()
    }

    private func start() {
        let cfg: HomeportConfig
        do {
            cfg = try Self.ensureConfig()
        } catch {
            status = "No configuration: \(error.localizedDescription)"
            return
        }
        server = cfg.server
        addDomain()
        do {
            core = try Core(config: CoreConfig(
                server: cfg.server, fingerprint: cfg.fingerprint, clientId: cfg.clientId,
                cachePath: nil, requestDeadlineMs: 1000, workerThreads: 1))
        } catch {
            status = "Client failed: \(error)"
            return
        }
        subscription = core?.subscribe(listener: ChangeForwarder { [weak self] in
            Task { @MainActor in self?.signalWorkingSetSoon() }
        })
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
        tick()
    }

    /// Loads config.json, creating it on first launch from the local server's
    /// certificate fingerprint.
    static func ensureConfig() throws -> HomeportConfig {
        if let c = HomeportConfig.load() { return c }
        let support = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Homeport")
        let candidates = ["client/server.sha256", "server/cert.sha256"].map { support.appendingPathComponent($0) }
        guard let fp = candidates.lazy.compactMap({ try? String(contentsOf: $0, encoding: .utf8) })
            .map({ $0.trimmingCharacters(in: .whitespacesAndNewlines) }).first(where: { $0.count == 64 })
        else {
            throw NSError(domain: "Homeport", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "no server fingerprint found; put it in \(HomeportConfig.configURL?.path ?? "config.json")",
            ])
        }
        let c = HomeportConfig(server: HomeportConfig.defaultServer, fingerprint: fp,
                               clientId: "fp-" + UUID().uuidString.lowercased())
        try c.save()
        log.notice("wrote config.json for \(c.server, privacy: .public)")
        return c
    }

    func addDomain() {
        NSFileProviderManager.add(domain) { error in
            Task { @MainActor in
                if let error {
                    log.error("add domain failed: \(String(describing: error), privacy: .public)")
                    self.domainError = error.localizedDescription
                } else {
                    self.domainError = nil
                }
            }
        }
    }

    private func tick() {
        guard let core else { return }
        let l = core.link()
        let state = l.probed ? l.state : nil
        if linkState != state { linkState = state }
        let text = Self.describe(l)
        if status != text { status = text }
        guard l.probed else { return }
        let offline = l.state == .offline
        defer { wasOffline = offline }
        guard let was = wasOffline, was != offline else { return }
        log.notice("link is now \(offline ? "offline" : "online", privacy: .public)")
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

    /// Removes the domain (and its replica and cache) and adds it again.
    func resetCache() {
        status = "Resetting…"
        NSFileProviderManager.remove(domain, mode: .removeAll) { _, error in
            Task { @MainActor in
                if let error { log.error("remove domain: \(String(describing: error), privacy: .public)") }
                if let cache = HomeportConfig.cacheURL?.deletingLastPathComponent() {
                    try? FileManager.default.removeItem(at: cache)
                }
                self.addDomain()
            }
        }
    }
}
