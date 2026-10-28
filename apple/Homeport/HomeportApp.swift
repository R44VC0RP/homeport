import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set once the model exists; links opened before that wait here.
    var onOpen: ((URL) -> Void)? {
        didSet { if let onOpen { pending.forEach(onOpen); pending.removeAll() } }
    }
    private var pending: [URL] = []

    func application(_ application: NSApplication, open urls: [URL]) {
        for url in urls {
            if let onOpen { onOpen(url) } else { pending.append(url) }
        }
    }
}

@main
struct HomeportApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()

    var body: some Scene {
        MenuBarExtra {
            MenuContent(model: model)
                .onAppear { connectLinks() }
        } label: {
            Image(systemName: symbol)
                .onAppear { connectLinks() }
        }
    }

    private func connectLinks() {
        guard delegate.onOpen == nil else { return }
        let model = self.model
        delegate.onOpen = { url in Task { @MainActor in model.open(url) } }
    }

    private var symbol: String {
        guard let state = model.worstLink else { return "externaldrive.badge.questionmark" }
        switch state {
        case .good: return "externaldrive.connected.to.line.below"
        case .degraded, .poor: return "externaldrive.badge.exclamationmark"
        case .offline: return "externaldrive.badge.xmark"
        }
    }
}

private struct MenuContent: View {
    @ObservedObject var model: AppModel

    var body: some View {
        if model.extensionState == .disabled {
            Button("Turn On Homeport in Finder…") { model.showOnboarding() }
            Divider()
        }
        if model.servers.isEmpty {
            Text("No servers")
        }
        ForEach(model.servers) { s in
            ServerMenu(model: model, server: s)
        }
        Divider()
        Button("Add Server…") { model.showAddServer() }
        Divider()
        Button("Quit Homeport") { NSApplication.shared.terminate(nil) }
            .keyboardShortcut("q")
    }
}

private struct ServerMenu: View {
    @ObservedObject var model: AppModel
    @ObservedObject var server: ServerController

    var body: some View {
        Menu("\(server.entry.name) — \(server.status)") {
            Text(server.details)
            if let err = server.domainError {
                Text("Finder: \(err)")
            }
            Divider()
            Button("Reveal in Finder") { server.revealInFinder() }
            Button("Rename…") { model.rename(server) }
            Button("Reset Cache") { server.resetCache() }
            Divider()
            Button("Remove…") { model.confirmRemove(server) }
        }
    }
}
