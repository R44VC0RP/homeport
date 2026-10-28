import SwiftUI

/// First run: turn the extension on, add a server, explain Finder access.
struct OnboardingView: View {
    @ObservedObject var model: AppModel

    private enum Step: Int, CaseIterable {
        case enable, addServer, finder
    }

    @State private var step: Step = .enable
    @State private var finderPermission: FinderWatcher.Permission = .unknown
    @State private var asking = false

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 6) {
                ForEach(Step.allCases, id: \.self) { s in
                    Capsule()
                        .fill(s.rawValue <= step.rawValue ? Color.accentColor : Color.secondary.opacity(0.25))
                        .frame(height: 4)
                }
            }
            switch step {
            case .enable: enableStep
            case .addServer: addServerStep
            case .finder: finderStep
            }
        }
        .padding(24)
        .frame(width: 540, height: 600, alignment: .top)
        .onAppear { step = firstStep() }
        .task {
            // Notice when the extension is switched on in System Settings.
            while !Task.isCancelled {
                await model.refreshExtensionState()
                if step == .enable && model.extensionState == .enabled { advance() }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func firstStep() -> Step {
        if model.extensionState != .enabled { return .enable }
        if model.servers.isEmpty { return .addServer }
        return .finder
    }

    private func advance() {
        switch step {
        case .enable: step = model.servers.isEmpty ? .addServer : .finder
        case .addServer: step = .finder
        case .finder: finish()
        }
    }

    private func finish() {
        model.windows.close("onboarding")
    }

    private func header(_ symbol: String, _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Image(systemName: symbol)
                .font(.system(size: 32))
                .foregroundStyle(Color.accentColor)
                .frame(width: 40)
            VStack(alignment: .leading, spacing: 6) {
                Text(title).font(.title2).fontWeight(.semibold)
                Text(text).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    // MARK: Steps

    private var enableStep: some View {
        VStack(alignment: .leading, spacing: 18) {
            header("puzzlepiece.extension", "Turn on Homeport in Finder",
                   "macOS adds Finder extensions switched off. In System Settings, open General › Login Items & Extensions, click the info button next to File Providers (under Extensions) and turn on Homeport.")
            HStack(spacing: 8) {
                switch model.extensionState {
                case .enabled:
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                    Text("Homeport is on.")
                case .disabled:
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
                    Text("Homeport is off.")
                default:
                    ProgressView().controlSize(.small)
                    Text("Waiting for Homeport to be turned on…")
                }
            }
            Spacer(minLength: 0)
            HStack {
                Button("Skip") { advance() }
                Spacer()
                if model.extensionState == .enabled {
                    Button("Continue") { advance() }.keyboardShortcut(.defaultAction)
                } else {
                    Button("Open System Settings") { model.openExtensionSettings() }.keyboardShortcut(.defaultAction)
                }
            }
        }
    }

    private var addServerStep: some View {
        VStack(alignment: .leading, spacing: 18) {
            header("server.rack", "Add a server",
                   "Pick a computer running homeportd on your tailnet. Each server gets its own place in Finder's sidebar.")
            AddServerView(model: model, embedded: true) { advance() }
            Spacer(minLength: 0)
            HStack {
                Spacer()
                Button(model.servers.isEmpty ? "Later" : "Continue") { advance() }
            }
        }
    }

    private var finderStep: some View {
        VStack(alignment: .leading, spacing: 18) {
            header("folder.badge.gearshape", "Let Homeport follow Finder",
                   "Homeport reads which folder Finder shows, so it can download the folders and small files you are likely to open next. It never changes anything in Finder. macOS asks for this permission once.")
            HStack(spacing: 8) {
                switch finderPermission {
                case .allowed:
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                    Text("Allowed.")
                case .denied:
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                    Text("Not allowed. Change it in System Settings › Privacy & Security › Automation.")
                        .fixedSize(horizontal: false, vertical: true)
                default:
                    EmptyView()
                }
            }
            Spacer(minLength: 0)
            HStack {
                if finderPermission != .allowed && finderPermission != .denied {
                    Button("Not Now") { finish() }
                }
                Spacer()
                if finderPermission == .allowed || finderPermission == .denied {
                    Button("Done") { finish() }.keyboardShortcut(.defaultAction)
                } else {
                    Button("Allow…") { ask() }.keyboardShortcut(.defaultAction).disabled(asking)
                }
            }
        }
        .task { finderPermission = await Task.detached { FinderWatcher.permission(ask: false) }.value }
    }

    private func ask() {
        asking = true
        Task {
            finderPermission = await Task.detached { FinderWatcher.permission(ask: true) }.value
            asking = false
        }
    }
}
