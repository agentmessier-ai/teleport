import Foundation
import SQLite3

private let SQLITE_TRANSIENT = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

struct LiveSession: Identifiable {
    let session_id: String
    let pid: Int32
    let tty: String?
    let cwd: String?
    let source: String
    let last_seen_at: Int64
    let alias: String?

    var id: String { session_id }
    var displayAlias: String {
        if let a = alias, !a.isEmpty { return a }
        if let cwd = cwd, !cwd.isEmpty { return (cwd as NSString).lastPathComponent }
        return session_id
    }
    var runtimeLabel: String {
        let parts = session_id.split(separator: "/", maxSplits: 2)
        return parts.count >= 2 ? String(parts[1]) : "?"
    }
    var isHook: Bool { source == "hook" }
}

struct Peer: Identifiable {
    let id: String
    let name: String
    let trust: String
    let addr: String?
    let last_seen_at: Int64?
}

struct PokeLog: Identifiable {
    let id: String
    let to_session: String
    let from_machine: String
    let kind: String
    let body: String
    let created_at: Int64
    let read_at: Int64?
}

/// Which build each of the three separately-installed pieces is.
///
/// They drift, routinely and silently: installing binaries does not restart
/// the LaunchAgent, and the panel is a bundle installed by a different command
/// again. `running` is the only one that describes the process actually
/// serving peer requests, which is why a mismatch against `cli` is worth
/// saying out loud rather than leaving for someone to notice.
struct Versions {
    var running: String?
    var startedAt: Int64?
    var cli: String?
    var panel: String

    /// Mirrors `tp_core::compare_builds` — three outcomes, not a bool. A dirty
    /// tree cannot be compared at all, so warning on one would nag through
    /// every rebuild while a genuinely stale daemon looks identical.
    enum Match { case same, different, unknown }

    var daemonMatch: Match {
        guard let a = sha(running), let b = sha(cli) else { return .unknown }
        if a.contains("dirty") || b.contains("dirty") || a == "unknown" || b == "unknown" {
            return .unknown
        }
        return a == b ? .same : .different
    }

    /// The commit out of `0.1.0 (33349da, 2026-08-16)`.
    private func sha(_ line: String?) -> String? {
        guard let line,
              let open = line.firstIndex(of: "("),
              let comma = line[line.index(after: open)...].firstIndex(where: { $0 == "," || $0 == ")" })
        else { return nil }
        return String(line[line.index(after: open)..<comma])
    }
}

struct PanelState {
    var sessions: [LiveSession] = []
    var peers: [Peer] = []
    var recentPokes: [PokeLog] = []
    var tpdRunning: Bool = false
    var versions = Versions(panel: TeleportDB.panelVersion())
}

final class TeleportDB {
    static let path = NSString(string: "~/.teleport/teleport.db").expandingTildeInPath
    static let tpBin = "\(NSString(string: "~/.local/bin").expandingTildeInPath)/tp"

    private func withDB<T>(_ body: (OpaquePointer) -> T?) -> T? {
        var db: OpaquePointer?
        guard sqlite3_open_v2(Self.path, &db, SQLITE_OPEN_READWRITE, nil) == SQLITE_OK, let db else {
            sqlite3_close(db)
            return nil
        }
        defer { sqlite3_close(db) }
        sqlite3_busy_timeout(db, 3000)
        return body(db)
    }

    private func exec(_ db: OpaquePointer, _ sql: String, _ bind: (OpaquePointer) -> Void = { _ in }) {
        var stmt: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &stmt, nil) == SQLITE_OK, let stmt else { return }
        defer { sqlite3_finalize(stmt) }
        bind(stmt)
        sqlite3_step(stmt)
    }

    private func text(_ stmt: OpaquePointer?, _ i: Int32) -> String {
        guard let c = sqlite3_column_text(stmt, i) else { return "" }
        return String(cString: c)
    }

    private func optText(_ stmt: OpaquePointer?, _ i: Int32) -> String? {
        guard sqlite3_column_type(stmt, i) != SQLITE_NULL, let c = sqlite3_column_text(stmt, i) else { return nil }
        return String(cString: c)
    }

    private func optInt64(_ stmt: OpaquePointer?, _ i: Int32) -> Int64? {
        guard sqlite3_column_type(stmt, i) != SQLITE_NULL else { return nil }
        return sqlite3_column_int64(stmt, i)
    }

    func loadState() -> PanelState {
        var state = PanelState()
        state.tpdRunning = Self.isTpdListening()

        _ = withDB { db -> Bool? in
            self.exec(db, """
                CREATE TABLE IF NOT EXISTS terminal_alias (
                    cwd TEXT PRIMARY KEY, alias TEXT NOT NULL,
                    last_tty TEXT, last_pid INTEGER, updated_at INTEGER NOT NULL
                ) STRICT
                """)

            var stmt: OpaquePointer?
            let sessionSQL = """
                SELECT ls.session_id, ls.pid, ls.tty, ls.cwd, ls.source, ls.last_seen_at, ta.alias
                FROM live_session ls
                LEFT JOIN terminal_alias ta ON ls.cwd = ta.cwd
                ORDER BY ls.last_seen_at DESC
                """
            if sqlite3_prepare_v2(db, sessionSQL, -1, &stmt, nil) == SQLITE_OK {
                while sqlite3_step(stmt) == SQLITE_ROW {
                    state.sessions.append(LiveSession(
                        session_id: self.text(stmt, 0),
                        pid: Int32(sqlite3_column_int(stmt, 1)),
                        tty: self.optText(stmt, 2),
                        cwd: self.optText(stmt, 3),
                        source: self.text(stmt, 4),
                        last_seen_at: sqlite3_column_int64(stmt, 5),
                        alias: self.optText(stmt, 6)
                    ))
                }
            }
            sqlite3_finalize(stmt)

            stmt = nil
            if sqlite3_prepare_v2(db, "SELECT id, name, trust, addr, last_seen_at FROM machine WHERE trust != 'self' ORDER BY name", -1, &stmt, nil) == SQLITE_OK {
                while sqlite3_step(stmt) == SQLITE_ROW {
                    state.peers.append(Peer(
                        id: self.text(stmt, 0),
                        name: self.text(stmt, 1),
                        trust: self.text(stmt, 2),
                        addr: self.optText(stmt, 3),
                        last_seen_at: self.optInt64(stmt, 4)
                    ))
                }
            }
            sqlite3_finalize(stmt)

            stmt = nil
            if sqlite3_prepare_v2(db, "SELECT id, to_session, from_machine, kind, body, created_at, read_at FROM message ORDER BY created_at DESC LIMIT 12", -1, &stmt, nil) == SQLITE_OK {
                while sqlite3_step(stmt) == SQLITE_ROW {
                    state.recentPokes.append(PokeLog(
                        id: self.text(stmt, 0),
                        to_session: self.text(stmt, 1),
                        from_machine: self.text(stmt, 2),
                        kind: self.text(stmt, 3),
                        body: self.text(stmt, 4),
                        created_at: sqlite3_column_int64(stmt, 5),
                        read_at: self.optInt64(stmt, 6)
                    ))
                }
            }
            sqlite3_finalize(stmt)

            stmt = nil
            if sqlite3_prepare_v2(db, "SELECT version, started_at FROM daemon_status WHERE id = 1", -1, &stmt, nil) == SQLITE_OK {
                if sqlite3_step(stmt) == SQLITE_ROW {
                    state.versions.running = self.text(stmt, 0)
                    state.versions.startedAt = sqlite3_column_int64(stmt, 1)
                }
            }
            sqlite3_finalize(stmt)
            return true
        }
        state.versions.cli = Self.cliVersion()
        return state
    }

    /// This bundle's own version, stamped into Info.plist at `make bundle`.
    static func panelVersion() -> String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
    }

    /// The version of the `tp` binary ON DISK — deliberately not the same
    /// question as what the daemon is running, which is the whole point of
    /// comparing them.
    private static func cliVersion() -> String? {
        let out = runTp(["--version"])
        guard !out.isEmpty, !out.hasPrefix("failed to run tp") else { return nil }
        // "tp 0.1.0 (33349da, 2026-08-17)" → drop the leading binary name.
        return out.hasPrefix("tp ") ? String(out.dropFirst(3)) : out
    }

    func setAlias(cwd: String, alias: String) {
        _ = withDB { db -> Bool? in
            let now = Int64(Date().timeIntervalSince1970 * 1000)
            self.exec(db, "INSERT INTO terminal_alias(cwd, alias, updated_at) VALUES(?1, ?2, ?3) ON CONFLICT(cwd) DO UPDATE SET alias=excluded.alias, updated_at=excluded.updated_at") { stmt in
                sqlite3_bind_text(stmt, 1, cwd, -1, SQLITE_TRANSIENT)
                sqlite3_bind_text(stmt, 2, alias, -1, SQLITE_TRANSIENT)
                sqlite3_bind_int64(stmt, 3, now)
            }
            return true
        }
    }

    static func isTpdListening() -> Bool {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
        task.arguments = ["-iTCP:47400", "-sTCP:LISTEN", "-t"]
        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = Pipe()
        do {
            try task.run()
            task.waitUntilExit()
            return !pipe.fileHandleForReading.readDataToEndOfFile().isEmpty
        } catch {
            return false
        }
    }

    static func restartTpd() {
        let uid = getuid()
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        task.arguments = ["kickstart", "-k", "gui/\(uid)/io.teleport.tpd"]
        try? task.run()
    }

    /// Enqueue a message and report what actually happened to it.
    ///
    /// This used to spawn `tp ask`, pipe both streams into a `Pipe` it never
    /// read, and not wait for the process — so the panel said "sent" for every
    /// outcome, including the ones `tp` names precisely: a target that is
    /// registered but has no injectable pane comes back "registered but not
    /// injectable — target checks on next /tp inbox", and a session in a
    /// terminal with no backend can only ever come back that way. Reported by
    /// the operator as "I poked it and it never arrived" — which was true, and
    /// the panel was the only component that did not know.
    static func poke(sessionId: String, message: String) -> String {
        runTp(["ask", sessionId, message])
    }

    /// Runs `tp <args>` and returns its combined stdout+stderr, trimmed.
    ///
    /// Every panel action goes through here: `tp` reports outcomes as text on
    /// those streams — "already trusted", "not trusted", "registered but not
    /// injectable" — and a panel that discards them is a panel that invents a
    /// result it did not get.
    private static func runTp(_ args: [String]) -> String {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: tpBin)
        task.arguments = args
        let out = Pipe()
        let err = Pipe()
        task.standardOutput = out
        task.standardError = err
        do {
            try task.run()
            task.waitUntilExit()
            let outText = String(data: out.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            let errText = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            return (outText + errText).trimmingCharacters(in: .whitespacesAndNewlines)
        } catch {
            return "failed to run tp: \(error.localizedDescription)"
        }
    }

    static func pairApprove(id: String) -> String { runTp(["pair", "approve", id]) }
    static func pairReject(id: String) -> String { runTp(["pair", "reject", id]) }
    static func pairRevoke(id: String) -> String { runTp(["pair", "revoke", id]) }

    static func focusTerminal(tty: String) {
        let ttyName = tty.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: "/dev/", with: "")
        let script = """
        tell application "iTerm2"
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        try
                            if (tty of s) ends with "\(ttyName)" then
                                set index of w to 1
                                select t
                                activate
                                return
                            end if
                        end try
                    end repeat
                end repeat
            end repeat
        end tell
        """
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        task.arguments = ["-e", script]
        try? task.run()
    }
}
