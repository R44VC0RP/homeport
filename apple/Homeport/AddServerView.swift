import SwiftUI

/// A server waiting for the person to confirm it before it is pinned.
private struct Confirmation {
    var probe: ProbeInfo
    var name: String
    /// The certificate matched a pairing link, so no code to compare.
    var verified: Bool
}

func osSymbol(_ os: String) -> String {
    switch os {
    case "macOS": return "desktopcomputer"
    case "Linux": return "server.rack"
    case "Windows": return "pc"
    default: return "externaldrive.connected.to.line.below"
    }
}

/// Add Server: servers found on the tailnet, a pairing link, or an address.
struct AddServerView: View {
    @ObservedObject var model: AppModel
    /// Shown inside onboarding (no Done button; `onDone` after adding).
    var embedded = false
    var onDone: (() -> Void)?

    @StateObject private var discovery = Discovery()
    @State private var link = ""
    @State private var showManual = false
    @State private var manualAddress = ""
    @State private var manualFingerprint = ""
    @State private var busy = false
    @State private var error: String?
    @State private var confirm: Confirmation?

    var body: some View {
        Group {
            if let c = confirm {
                confirmView(c)
            } else {
                chooser
            }
        }
        .padding(embedded ? 0 : 20)
        .frame(width: embedded ? nil : 480, height: embedded ? nil : 520, alignment: .top)
        .task { await discovery.scan() }
        .onAppear(perform: consumePendingLink)
        .onChange(of: model.pendingLink) { consumePendingLink() }
    }

    // MARK: Choosing

    private var chooser: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("Servers on your tailnet").font(.headline)
                Spacer()
                if discovery.scanning {
                    ProgressView().controlSize(.small)
                } else {
                    Button { Task { await discovery.scan() } } label: { Image(systemName: "arrow.clockwise") }
                        .buttonStyle(.borderless)
                        .help("Search again")
                }
            }
            VStack(spacing: 0) {
                if discovery.found.isEmpty {
                    Text(discovery.scanning ? "Searching…" : (discovery.note ?? "No servers found."))
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .padding()
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    ScrollView {
                        VStack(spacing: 0) {
                            ForEach(discovery.found) { f in
                                row(f)
                                if f.id != discovery.found.last?.id { Divider() }
                            }
                        }
                    }
                }
            }
            .frame(height: embedded ? 150 : 220)
            .background(.background, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))

            VStack(alignment: .leading, spacing: 6) {
                Text("Pairing link").font(.headline)
                HStack {
                    TextField("homeport://pair?… or HOMEPORT:…", text: $link)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(addFromLink)
                    Button("Add", action: addFromLink).disabled(link.isEmpty || busy)
                }
                Text("Run `homeportd pair` on the server to get one.")
                    .font(.caption).foregroundStyle(.secondary)
            }

            DisclosureGroup("Enter an address", isExpanded: $showManual) {
                VStack(alignment: .leading, spacing: 8) {
                    TextField("Tailnet IP or name, e.g. 100.101.102.103", text: $manualAddress)
                        .textFieldStyle(.roundedBorder)
                    TextField("Certificate fingerprint (optional)", text: $manualFingerprint)
                        .textFieldStyle(.roundedBorder)
                        .font(.system(.body, design: .monospaced))
                    HStack {
                        Text("Without a fingerprint you compare a code instead.")
                            .font(.caption).foregroundStyle(.secondary)
                        Spacer()
                        Button("Connect", action: addManual).disabled(manualAddress.isEmpty || busy)
                    }
                }
                .padding(.top, 6)
            }

            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if busy { ProgressView().controlSize(.small) }

            Spacer(minLength: 0)
            if !embedded {
                HStack {
                    Spacer()
                    Button("Done") { model.windows.close("add-server") }
                        .keyboardShortcut(.cancelAction)
                }
            }
        }
    }

    private func row(_ f: FoundServer) -> some View {
        HStack(spacing: 10) {
            Image(systemName: osSymbol(f.probe?.info.os ?? ""))
                .font(.title2)
                .frame(width: 28)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(f.probe.map { defaultServerName($0.info) } ?? f.host).fontWeight(.medium)
                Text(subtitle(f)).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            if let p = f.probe {
                if let existing = model.server(withFingerprint: p.fingerprint) {
                    Text(existing.entry.name == defaultServerName(p.info) ? "Added" : "Added as \(existing.entry.name)")
                        .foregroundStyle(.secondary)
                } else {
                    Button("Add…") { confirm = Confirmation(probe: p, name: defaultServerName(p.info), verified: false) }
                }
            } else if let problem = f.problem {
                Text(problem).font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func subtitle(_ f: FoundServer) -> String {
        guard let p = f.probe else { return f.address }
        var parts = [p.info.os, p.address.replacingOccurrences(of: ":\(Discovery.port)", with: "")]
        if !p.shares.isEmpty { parts.append(p.shares.joined(separator: ", ")) }
        parts.append(String(format: "%.0f ms", p.elapsedMs))
        return parts.joined(separator: " · ")
    }

    // MARK: Confirming

    private func confirmView(_ c: Confirmation) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 12) {
                Image(systemName: osSymbol(c.probe.info.os)).font(.largeTitle).foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Add \(defaultServerName(c.probe.info))?").font(.title2).fontWeight(.semibold)
                    Text("\(c.probe.info.os) · homeportd \(c.probe.info.version) · \(c.probe.address)")
                        .font(.caption).foregroundStyle(.secondary)
                    if !c.probe.shares.isEmpty {
                        Text("Shares: \(c.probe.shares.joined(separator: ", "))")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            if c.verified {
                Label("The server's certificate matches the pairing link.", systemImage: "checkmark.seal.fill")
                    .foregroundStyle(.green)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Check that the server shows the same code:")
                    Text(c.probe.code)
                        .font(.system(size: 40, weight: .semibold, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 6)
                    Text("Run `homeportd pair` on \(defaultServerName(c.probe.info)) to see its code. If they differ, something else is answering at this address: don't add it.")
                        .font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            VStack(alignment: .leading, spacing: 4) {
                Text("Name in Finder").font(.caption).foregroundStyle(.secondary)
                TextField("Name", text: Binding(
                    get: { confirm?.name ?? "" }, set: { confirm?.name = $0 }))
                    .textFieldStyle(.roundedBorder)
            }
            Spacer(minLength: 0)
            HStack {
                Button("Cancel") { confirm = nil }.keyboardShortcut(.cancelAction)
                Spacer()
                Button(c.verified ? "Add Server" : "Codes Match, Add Server") { finish(c) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(c.name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func finish(_ c: Confirmation) {
        let name = (confirm?.name ?? c.name).trimmingCharacters(in: .whitespacesAndNewlines)
        model.add(probe: c.probe, name: name)
        confirm = nil
        link = ""
        if let onDone {
            onDone()
        } else {
            model.windows.close("add-server")
        }
    }

    // MARK: Links and addresses

    private func consumePendingLink() {
        guard let l = model.pendingLink else { return }
        model.pendingLink = nil
        link = l
        addFromLink(l)
    }

    private func addFromLink() { addFromLink(link) }

    private func addFromLink(_ text: String) {
        guard let target = PairingTarget.parse(text) else {
            error = "That isn't a Homeport pairing link. It starts with homeport://pair or HOMEPORT:."
            return
        }
        connect(target)
    }

    private func addManual() {
        let fp = manualFingerprint.filter { !$0.isWhitespace && $0 != ":" }.lowercased()
        if !fp.isEmpty && !PairingTarget.isFingerprint(fp) {
            error = "A fingerprint is 64 hex digits (homeportd info shows it)."
            return
        }
        connect(PairingTarget(address: manualAddress, fingerprint: fp.isEmpty ? nil : fp, name: nil))
    }

    private func connect(_ target: PairingTarget) {
        error = nil
        busy = true
        Task {
            defer { busy = false }
            do {
                let p = try await probeServer(address: target.address, timeoutMs: 4000)
                if let expected = target.fingerprint, expected != p.fingerprint.lowercased() {
                    error = "The server at \(p.address) presents a different certificate than expected (\(p.fingerprint.prefix(8))…, not \(expected.prefix(8))…). Don't add it unless you know why it changed."
                    return
                }
                if let existing = model.server(withFingerprint: p.fingerprint) {
                    let name = defaultServerName(p.info)
                    error = existing.entry.name == name ? "\(name) is already added."
                        : "\(name) is already added as \(existing.entry.name)."
                    return
                }
                confirm = Confirmation(probe: p, name: target.name ?? defaultServerName(p.info),
                                       verified: target.fingerprint != nil)
            } catch CoreError.Denied(_) {
                error = "\(target.address) doesn't allow your Tailscale login. Add it on the server: homeportd allows logins listed in allow_logins of server.toml."
            } catch CoreError.Unreachable(_) {
                error = "Nothing answered at \(target.address). Check that homeportd runs there and Tailscale is connected."
            } catch {
                self.error = "Couldn't reach \(target.address): \(String(describing: error))"
            }
        }
    }
}
