import SwiftUI

@main
struct HomeportApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        MenuBarExtra {
            Text("basebook: \(model.status)")
            Text("Server \(model.server)")
            if let err = model.domainError {
                Text("Finder: \(err)")
            }
            Divider()
            Button("Reveal in Finder") { model.revealInFinder() }
            Button("Reset Cache") { model.resetCache() }
            Divider()
            Button("Quit Homeport") { NSApplication.shared.terminate(nil) }
                .keyboardShortcut("q")
        } label: {
            Image(systemName: symbol)
        }
    }

    private var symbol: String {
        guard let l = model.link, l.probed else { return "externaldrive.badge.questionmark" }
        switch l.state {
        case .good: return "externaldrive.connected.to.line.below"
        case .degraded, .poor: return "externaldrive.badge.exclamationmark"
        case .offline: return "externaldrive.badge.xmark"
        }
    }
}
