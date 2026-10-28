import Foundation
import os

private let log = Logger(subsystem: "dev.vogel.homeport", category: "discovery")

/// What a pairing link or the manual form names: an address and (from a
/// link) the certificate to expect.
struct PairingTarget: Equatable {
    var address: String
    var fingerprint: String?
    var name: String?

    /// `homeport://pair?addr=…&fp=…&name=…` or `HOMEPORT:<FP>:<ADDR>`.
    static func parse(_ text: String) -> PairingTarget? {
        let s = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.uppercased().hasPrefix("HOMEPORT:") && !s.lowercased().hasPrefix("homeport://") {
            let rest = s.dropFirst("HOMEPORT:".count)
            guard rest.count > 65, let colon = rest.firstIndex(of: ":"),
                  rest.distance(from: rest.startIndex, to: colon) == 64 else { return nil }
            let fp = String(rest[..<colon]).lowercased()
            let addr = String(rest[rest.index(after: colon)...])
            guard isFingerprint(fp), !addr.isEmpty else { return nil }
            return PairingTarget(address: addr, fingerprint: fp, name: nil)
        }
        guard let c = URLComponents(string: s), c.scheme?.lowercased() == "homeport" else { return nil }
        let q = Dictionary((c.queryItems ?? []).compactMap { i in i.value.map { (i.name, $0) } }) { a, _ in a }
        guard let addr = q["addr"], !addr.isEmpty else { return nil }
        let fp = q["fp"]?.lowercased()
        if let fp, !isFingerprint(fp) { return nil }
        return PairingTarget(address: addr, fingerprint: fp, name: q["name"])
    }

    static func isFingerprint(_ s: String) -> Bool {
        s.count == 64 && s.allSatisfy(\.isHexDigit)
    }
}

/// The name Finder shows for a server by default. Windows reports its
/// NetBIOS name in capitals ("EXON-DESK"); people know it in lower case.
func defaultServerName(_ info: FfiServerInfo) -> String {
    if info.os == "Windows" && info.name == info.name.uppercased() { return info.name.lowercased() }
    return info.name
}

/// A tailnet node that answered (or refused) a Homeport probe.
struct FoundServer: Identifiable, Equatable {
    var id: String { address }
    /// Tailscale's name for the node.
    var host: String
    var address: String
    var probe: ProbeInfo?
    /// Why it cannot be added (for example, it does not allow this login).
    var problem: String?

    static func == (a: FoundServer, b: FoundServer) -> Bool {
        a.address == b.address && a.probe?.fingerprint == b.probe?.fingerprint && a.problem == b.problem
    }
}

/// Finds Homeport servers on the tailnet: lists online peers with the
/// Tailscale CLI and asks each one's port 47820 who it is, in parallel.
@MainActor
final class Discovery: ObservableObject {
    @Published private(set) var found: [FoundServer] = []
    @Published private(set) var scanning = false
    @Published private(set) var note: String?

    nonisolated static let port = 47820
    nonisolated private static let cliCandidates = [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
    ]

    struct Peer {
        var host: String
        var ip: String
    }

    func scan() async {
        guard !scanning else { return }
        scanning = true
        note = nil
        defer { scanning = false }
        let started = Date()
        let peers: [Peer]
        do {
            peers = try await Self.tailnetPeers()
        } catch {
            note = error.localizedDescription
            return
        }
        var results: [FoundServer] = []
        await withTaskGroup(of: FoundServer?.self) { group in
            for p in peers {
                group.addTask {
                    let address = "\(p.ip):\(Self.port)"
                    do {
                        let probe = try await probeServer(address: address, timeoutMs: 2500)
                        return FoundServer(host: p.host, address: address, probe: probe, problem: nil)
                    } catch CoreError.Denied(_) {
                        return FoundServer(host: p.host, address: address, probe: nil,
                                           problem: "Doesn't allow your Tailscale login")
                    } catch CoreError.Server(let msg) where msg.contains("protocol") {
                        return FoundServer(host: p.host, address: address, probe: nil,
                                           problem: "Needs a newer homeportd")
                    } catch {
                        return nil
                    }
                }
            }
            for await r in group {
                if let r {
                    results.append(r)
                    found = results.sorted { ($0.probe == nil ? 1 : 0, $0.host) < ($1.probe == nil ? 1 : 0, $1.host) }
                }
            }
        }
        found = results.sorted { ($0.probe == nil ? 1 : 0, $0.host) < ($1.probe == nil ? 1 : 0, $1.host) }
        if results.isEmpty {
            note = "No Homeport servers answered on \(peers.count) online tailnet devices."
        }
        log.notice("probed \(peers.count) peers in \(Int(Date().timeIntervalSince(started) * 1000)) ms: \(results.count) servers")
    }

    /// Online tailnet nodes (this Mac included) with their first IPv4.
    nonisolated static func tailnetPeers() async throws -> [Peer] {
        guard let cli = cliCandidates.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) else {
            throw NSError(domain: "Homeport", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "Tailscale isn't installed. Homeport finds servers through Tailscale; install it from tailscale.com.",
            ])
        }
        let data = try await Task.detached { () throws -> Data in
            let p = Process()
            p.executableURL = URL(fileURLWithPath: cli)
            p.arguments = ["status", "--json"]
            let out = Pipe()
            p.standardOutput = out
            p.standardError = Pipe()
            try p.run()
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            return data
        }.value
        guard let json = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw NSError(domain: "Homeport", code: 2, userInfo: [NSLocalizedDescriptionKey: "Tailscale gave no status. Is it running and signed in?"])
        }
        if let state = json["BackendState"] as? String, state != "Running" {
            throw NSError(domain: "Homeport", code: 3, userInfo: [
                NSLocalizedDescriptionKey: "Tailscale is \(state.lowercased()). Connect it, then search again.",
            ])
        }
        var nodes: [[String: Any]] = []
        if let me = json["Self"] as? [String: Any] { nodes.append(me) }
        if let peers = json["Peer"] as? [String: [String: Any]] { nodes += peers.values }
        return nodes.compactMap { n in
            guard (n["Online"] as? Bool) ?? false else { return nil }
            guard let ip = (n["TailscaleIPs"] as? [String])?.first(where: { $0.contains(".") }) else { return nil }
            let dns = (n["DNSName"] as? String)?.split(separator: ".").first.map(String.init)
            let host = dns ?? (n["HostName"] as? String) ?? ip
            return Peer(host: host, ip: ip)
        }
    }
}
