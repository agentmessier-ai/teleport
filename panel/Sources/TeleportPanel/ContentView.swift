import SwiftUI

/// A pairing action waiting on the user to say yes. Holds its own closure so
/// the dialog does not have to re-derive which peer and which verb it was
/// raised for — the row that raised it is gone from view by then.
struct PendingConfirm {
    let title: String
    let message: String
    let verb: String
    let action: () -> String
}

struct ContentView: View {
    @State private var state = PanelState()
    @State private var selectedSessionId = ""
    @State private var pokeText = ""
    @State private var renameText = ""
    @State private var renaming = false
    @State private var pokeResult = ""
    @State private var pairResult = ""
    @State private var pendingConfirm: PendingConfirm?
    private let db = TeleportDB()
    private let timer = Timer.publish(every: 2, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header

            Divider()

            sessionList

            if let sel = selected {
                actionRow(sel)
                if renaming { renameRow(sel) }
                pokeRow(sel)
            }

            Divider()
            peerConfirmDialog(peerSection)

            if !state.recentPokes.isEmpty {
                Divider()
                pokeLogSection
            }

            Divider()
            footer
        }
        .padding(12)
        .frame(width: 340)
        .onAppear { refresh() }
        .onReceive(timer) { _ in refresh() }
    }

    private var selected: LiveSession? {
        state.sessions.first { $0.session_id == selectedSessionId }
    }

    private var header: some View {
        HStack {
            Circle()
                .fill(state.tpdRunning ? Color.green : Color.red)
                .frame(width: 8, height: 8)
            Text("Teleport")
                .font(.headline)
            Spacer()
            Text(state.tpdRunning ? "tpd :47400" : "tpd down")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var sessionList: some View {
        VStack(spacing: 3) {
            ForEach(state.sessions) { s in
                Button { selectedSessionId = s.session_id } label: {
                    HStack(spacing: 6) {
                        Image(systemName: selectedSessionId == s.session_id ? "circle.fill" : "circle")
                            .font(.system(size: 10))
                            .foregroundColor(selectedSessionId == s.session_id ? .accentColor : .secondary)
                        VStack(alignment: .leading, spacing: 1) {
                            HStack(spacing: 4) {
                                Text(s.displayAlias)
                                    .font(.caption)
                                    .fontWeight(.medium)
                                if s.isHook {
                                    Text("hook")
                                        .font(.system(size: 8))
                                        .padding(.horizontal, 3)
                                        .background(Color.accentColor.opacity(0.2))
                                        .clipShape(RoundedRectangle(cornerRadius: 2))
                                }
                                Spacer()
                                Text("\(s.runtimeLabel)")
                                    .font(.system(size: 9))
                                    .foregroundStyle(.secondary)
                            }
                            Text("\(s.tty ?? "?") · pid \(s.pid)")
                                .font(.system(size: 8))
                                .foregroundStyle(.tertiary)
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func actionRow(_ s: LiveSession) -> some View {
        HStack(spacing: 8) {
            Button("rename") { renaming.toggle(); renameText = s.alias ?? (s.cwd.map { ($0 as NSString).lastPathComponent } ?? "") }
            if let tty = s.tty {
                Button("focus") { TeleportDB.focusTerminal(tty: tty) }
            }
            Spacer()
        }
        .font(.caption)
    }

    private func renameRow(_ s: LiveSession) -> some View {
        HStack {
            TextField("alias", text: $renameText)
                .textFieldStyle(.roundedBorder)
                .font(.caption)
            Button("save") {
                if let cwd = s.cwd { db.setAlias(cwd: cwd, alias: renameText) }
                renaming = false
                refresh()
            }
        }
        .font(.caption)
    }

    private func pokeRow(_ s: LiveSession) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                TextField("poke \(s.displayAlias)…", text: $pokeText)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                    .onSubmit { sendPoke(s) }
                Button("→") { sendPoke(s) }
            }
            if !pokeResult.isEmpty {
                Text(pokeResult)
                    .font(.system(size: 9))
                    .foregroundStyle(pokeResult.contains("not injectable") ? .orange : .secondary)
                    // `tp`'s outcome line wraps; truncating it would hide the
                    // half that says whether anyone will see the message.
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func sendPoke(_ s: LiveSession) {
        let msg = pokeText.trimmingCharacters(in: .whitespaces)
        guard !msg.isEmpty else { return }
        // Whatever `tp` says, verbatim — it distinguishes queued-and-woken
        // from queued-but-unwakeable, and only one of those means the target
        // will see it without being asked to look.
        pokeResult = TeleportDB.poke(sessionId: s.session_id, message: msg)
        pokeText = ""
        DispatchQueue.main.asyncAfter(deadline: .now() + 8) { pokeResult = "" }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { refresh() }
    }

    private var peerSection: some View {
        VStack(alignment: .leading, spacing: 6) {
            if state.peers.isEmpty {
                Text("no peers")
                    .font(.system(size: 9))
                    .foregroundStyle(.tertiary)
            } else {
                ForEach(state.peers) { p in
                    peerRow(p)
                }
            }
            if !pairResult.isEmpty {
                Text(pairResult)
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func peerRow(_ p: Peer) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 4) {
                Circle()
                    .fill(peerColor(p.trust))
                    .frame(width: 5, height: 5)
                Text("\(p.name)  \(p.addr ?? "?")")
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                Spacer()
                Text(peerLabel(p.trust))
                    .font(.system(size: 8))
                    .foregroundStyle(.tertiary)
            }
            // The device id, unabbreviated — this is what a human compares
            // against `tp id` on the other machine before approving, so
            // truncating it would hide exactly the bytes that matter.
            Text(p.id)
                .font(.system(size: 8, design: .monospaced))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
            peerActions(p)
        }
    }

    private func peerColor(_ trust: String) -> Color {
        switch trust {
        case "trusted": return .green
        case "pending_in", "pending_out": return .orange
        default: return .secondary
        }
    }

    private func peerLabel(_ trust: String) -> String {
        switch trust {
        case "pending_in": return "wants in"
        case "pending_out": return "waiting on them"
        case "trusted": return "trusted"
        default: return trust
        }
    }

    @ViewBuilder
    private func peerActions(_ p: Peer) -> some View {
        switch p.trust {
        case "pending_in":
            VStack(alignment: .leading, spacing: 2) {
                Text("compare the id above with `tp id` on their machine first")
                    .font(.system(size: 8))
                    .foregroundStyle(.tertiary)
                HStack(spacing: 8) {
                    Button("approve") {
                        confirm(
                            "Trust \(p.name)?",
                            "It will be able to read every session on this machine. Only approve if the device id above matches what \(p.name) shows for itself.",
                            "Trust"
                        ) { TeleportDB.pairApprove(id: p.id) }
                    }
                    Button("reject") {
                        confirm(
                            "Refuse \(p.name)?",
                            "The request is removed. \(p.name) is not told, and may ask again later.",
                            "Refuse"
                        ) { TeleportDB.pairReject(id: p.id) }
                    }
                }
                .font(.system(size: 9))
            }
        case "trusted":
            HStack {
                Button("revoke") {
                    confirm(
                        "Revoke \(p.name)?",
                        "It loses access on its very next request. The relationship is removed entirely — pairing again means approving it from scratch. \(p.name) is not notified.",
                        "Revoke"
                    ) { TeleportDB.pairRevoke(id: p.id) }
                }
                Spacer()
            }
            .font(.system(size: 9))
        default:
            EmptyView()
        }
    }

    /// Every pairing button goes through here. These are the only actions in
    /// the panel that change who may read this machine's transcripts, and a
    /// popover is a place where a stray click lands easily — the popover can
    /// even be dismissed by clicking away, so a mis-click has no undo.
    private func confirm(
        _ title: String,
        _ message: String,
        _ verb: String,
        action: @escaping () -> String
    ) {
        pendingConfirm = PendingConfirm(
            title: title, message: message, verb: verb, action: action)
    }

    private func peerConfirmDialog<V: View>(_ content: V) -> some View {
        content.confirmationDialog(
            pendingConfirm?.title ?? "",
            isPresented: Binding(
                get: { pendingConfirm != nil },
                set: { if !$0 { pendingConfirm = nil } }
            ),
            titleVisibility: .visible
        ) {
            if let c = pendingConfirm {
                Button(c.verb, role: c.verb == "Trust" ? nil : .destructive) {
                    runPair(c.action)
                }
            }
            Button("Cancel", role: .cancel) { pendingConfirm = nil }
        } message: {
            Text(pendingConfirm?.message ?? "")
        }
    }

    private func runPair(_ action: () -> String) {
        let result = action()
        pendingConfirm = nil
        pairResult = result.isEmpty ? "done" : result
        DispatchQueue.main.asyncAfter(deadline: .now() + 4) { pairResult = "" }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { refresh() }
    }

    private var pokeLogSection: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Recent pokes")
                .font(.system(size: 9))
                .foregroundStyle(.secondary)
            ForEach(state.recentPokes.prefix(6)) { p in
                HStack(spacing: 4) {
                    Text(fmtTime(p.created_at))
                        .font(.system(size: 8))
                        .foregroundStyle(.tertiary)
                    Text("[\(p.kind)]")
                        .font(.system(size: 8))
                        .foregroundStyle(.secondary)
                    Text(p.body.prefix(40))
                        .font(.system(size: 8))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    if p.read_at != nil {
                        Image(systemName: "checkmark")
                            .font(.system(size: 7))
                            .foregroundStyle(.green)
                    }
                }
            }
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 4) {
            versionSection
            HStack {
                Button("restart tpd") {
                    TeleportDB.restartTpd()
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1) { refresh() }
                }
                Spacer()
                Button("quit") { NSApplication.shared.terminate(nil) }
            }
            .font(.caption)
        }
    }

    private var versionSection: some View {
        let v = state.versions
        return VStack(alignment: .leading, spacing: 1) {
            versionRow("tpd", v.running.map { r in
                v.startedAt.map { "\(r) · up \(uptime($0))" } ?? r
            } ?? "not recorded")
            versionRow("tp", v.cli ?? "not found")
            versionRow("panel", v.panel)

            // The one case worth interrupting for: new binaries are installed
            // but the LaunchAgent still has the old ones mapped, so everything
            // on disk looks current while the daemon serving peers is not.
            if v.daemonMatch == .different {
                Text("tpd is running different code than the installed binary — restart it")
                    .font(.system(size: 9))
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func versionRow(_ label: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            Text(label)
                .font(.system(size: 8))
                .foregroundStyle(.tertiary)
                .frame(width: 30, alignment: .leading)
            Text(value)
                .font(.system(size: 8, design: .monospaced))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func uptime(_ startedAtSecs: Int64) -> String {
        let secs = max(0, Int64(Date().timeIntervalSince1970) - startedAtSecs)
        if secs < 60 { return "\(secs)s" }
        if secs < 3600 { return "\(secs / 60)m" }
        if secs < 86_400 { return "\(secs / 3600)h" }
        return "\(secs / 86_400)d"
    }

    private func refresh() {
        state = db.loadState()
        if selectedSessionId.isEmpty, let first = state.sessions.first {
            selectedSessionId = first.session_id
        }
    }

    private func fmtTime(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        let fmt = DateFormatter()
        fmt.dateFormat = "HH:mm"
        return fmt.string(from: date)
    }
}
