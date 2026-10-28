import AppKit
import os

private let log = Logger(subsystem: "dev.vogel.homeport", category: "finder")

/// Polls Finder's front window (folder and selection) with AppleScript while
/// Finder is the frontmost app, and reports changes.
final class FinderWatcher {
    private let queue = DispatchQueue(label: "dev.vogel.homeport.finder")
    private var timer: DispatchSourceTimer?
    private var last = ""
    private let onChange: (URL, [URL]) -> Void
    private var denied = false

    private static let source = """
    tell application "Finder"
        set out to ""
        try
            set out to POSIX path of (target of front Finder window as alias)
        end try
        try
            set sel to selection
            if (count of sel) > 16 then set sel to items 1 thru 16 of sel
            repeat with s in sel
                set out to out & linefeed & POSIX path of (s as alias)
            end repeat
        end try
        return out
    end tell
    """

    /// Compiled once and only used on `queue`.
    private lazy var script = NSAppleScript(source: Self.source)

    init(onChange: @escaping (URL, [URL]) -> Void) {
        self.onChange = onChange
    }

    func start() {
        queue.async { Self.logPermission() }
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + 1, repeating: .milliseconds(400), leeway: .milliseconds(100))
        t.setEventHandler { [weak self] in self?.poll() }
        t.resume()
        timer = t
    }

    enum Permission {
        case allowed, denied, undecided, unknown
    }

    /// Whether Homeport may send Apple events to Finder. With `ask`, macOS
    /// shows its consent prompt if the person has not decided yet (this
    /// blocks until they answer: call it off the main thread).
    static func permission(ask: Bool) -> Permission {
        let target = NSAppleEventDescriptor(bundleIdentifier: "com.apple.finder")
        guard let desc = target.aeDesc else { return .unknown }
        switch AEDeterminePermissionToAutomateTarget(desc, typeWildCard, typeWildCard, ask) {
        case noErr: return .allowed
        case OSStatus(errAEEventNotPermitted): return .denied
        case OSStatus(errAEEventWouldRequireUserConsent): return .undecided
        default: return .unknown
        }
    }

    /// Logs whether Homeport may send Apple events to Finder, without asking.
    private static func logPermission() {
        let target = NSAppleEventDescriptor(bundleIdentifier: "com.apple.finder")
        guard let desc = target.aeDesc else { return }
        let status = AEDeterminePermissionToAutomateTarget(desc, typeWildCard, typeWildCard, false)
        let state: String
        switch status {
        case noErr: state = "allowed"
        case OSStatus(errAEEventNotPermitted): state = "denied (System Settings > Privacy & Security > Automation)"
        case OSStatus(errAEEventWouldRequireUserConsent): state = "not decided yet (macOS asks the first time Finder is frontmost)"
        case OSStatus(procNotFound): state = "unknown (Finder not running)"
        default: state = "status \(status)"
        }
        log.notice("Finder automation: \(state, privacy: .public)")
    }

    private func poll() {
        guard !denied, NSWorkspace.shared.frontmostApplication?.bundleIdentifier == "com.apple.finder" else { return }
        var error: NSDictionary?
        guard let out = script?.executeAndReturnError(&error).stringValue else {
            if let code = error?[NSAppleScript.errorNumber] as? Int, code == -1743 {
                // Not authorized: stop asking until the next launch.
                denied = true
                log.error("Finder automation not allowed; only the background crawl runs")
            }
            return
        }
        guard out != last else { return }
        last = out
        let lines = out.split(separator: "\n").map(String.init)
        guard let first = lines.first, !first.isEmpty else { return }
        onChange(URL(fileURLWithPath: first), lines.dropFirst().map { URL(fileURLWithPath: $0) })
    }
}
