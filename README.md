# Teleport

Cross-agent, cross-machine **memory and reach** for AI coding agents.

Your agent sessions are already a written record of everything you've figured
out — and that record is write-only. It lives in per-session JSONL files no tool
reads back, so every new session starts from nothing and you re-explain what you
already solved last week in the next tab.

Teleport makes those sessions **addressable, searchable, and reachable**: search
what any agent discussed (including its `thinking`), read a conversation back by
time or by day, and message a live session — on this machine or on a trusted Mac
on your LAN.

Rust + SQLite + a LaunchAgent, macOS. Reads **Claude Code**, **Pi** and
**Codex** today, and reaches **dsh** sessions; a new runtime's transcripts are
taught to it with a TOML file rather than code.

```bash
# What was I doing this afternoon? (no session id needed — finding it IS the question)
tp turns --since 4h --folder ~/dev/myproject

# What happened on one specific day
tp sessions --since 2026-08-04 --until 2026-08-05                  # who was working then
tp search "oauth keychain" --since 2026-08-04 --until 2026-08-05   # where it was discussed
tp turns --since 2026-08-04 --until 2026-08-05 --folder ~/dev/myproject

# Where was this discussed, across every session on the machine
tp search "cache invalidation" --since 7d --include-thinking

# Message a live session
tp live                                                # what's running right now
tp ask SWSQ-…/pi/019fc929-… "which spec is authoritative?"

# Reach another machine — name it; there is no LAN-wide browse
tp discover 10.0.0.42
tp pair request 10.0.0.42:47400
tp pair approve 6BPH-S3AD-…
tp search "cache invalidation" --all                   # …and search it too
```

## What it does

| Capability | What you get |
|---|---|
| **Search** | Query every Claude Code *and* Pi transcript on this machine — or on a trusted peer — and get *coordinates + excerpt*, not whole dumps. `thinking` is searchable, opt-in, on both runtimes. |
| **Read** | Pull a real conversation back: one session, a time window, or one specific day. Bounded, and it tells you when it truncated and how to page. |
| **Reach** | `tp ask` enqueues a message into a *live* session's mailbox and wakes it — delegate work, not just ask questions. Content never crosses a pane as keystrokes; only a fixed `/tp inbox` control string does. The target does the work and `tp reply`s with what it did. |
| **Federate** | Probe a named host for a teleport daemon, pair with explicit human approval + out-of-band fingerprint comparison, then query them. Requests are signed (RFC 9421) over TLS. |

### The honest capability matrix

Not everything is shipped. This is what a fresh install actually does:

| Feature | Same machine | Across machines (LAN/VPN) |
|---|---|---|
| Search sessions + turns | ✅ scan-based, sub-second when scoped | ✅ fan-out to trusted peers |
| Search `thinking` | ✅ opt-in (`--include-thinking`) | ✅ opt-in |
| List live sessions | ✅ `tpd` active scan, authoritative | ❌ own machine only |
| Poke / message a live session | ✅ `tp ask` → tmux/iTerm2 wake | ❌ **not shipped** — designed (LLD §7–8), not wired |
| Type raw keystrokes | ✅ `tp type` — no safety gate, CLI-only | ❌ |

Per runtime:

All four are read through a shipped TOML descriptor — there is no hand-written
adapter left for any of them, so "supported" and "has a descriptor" mean the
same thing here.

| Runtime | Search history | Live + pokeable | Can *use* teleport | What `install.sh` wires |
|---|---|---|---|---|
| Claude Code | ✅ | ✅ hooks | ✅ MCP tools + skill | all of it, via `claude plugin install` |
| Pi | ✅ | ✅ extension | ✅ registered tools + skill | all of it, when `~/.pi/agent` exists |
| codex | ✅ | ✅ hooks | ✅ MCP tools + skill | the skill — hook and MCP entry by hand |
| dsh | ⚠️ from install onward | ✅ extension | ✅ registered tools + skill | the skill — extension by hand |

**One skill document, in a directory three of the four agree to read.**
`~/.agents/skills` is not teleport's convention: codex documents it as its User
scope, dsh calls it "the shared agent config root scanned for compatible
skills", and pi scans it alongside its own. So `install.sh` writes the skill
there once and pi, codex and dsh all find it. Claude Code is the exception — it
scans personal, project and plugin roots only — and gets the same file through
the plugin.

**Every harness here has its own package manager, and `install.sh` now uses
Claude Code's.** `claude plugin install` delivers the hooks, `/tp`, the skill and
the MCP server together, and `claude plugin uninstall` takes all four back
without this script's help — which is what replaced three hand-copies, a Python
file that merged JSON into your `settings.json`, and a cache-refresh step that
existed only because the plugin system was being bypassed.

codex (`codex plugin add`), pi (`pi install`) and dsh (`dsh plugin add`) have the
same shape and have NOT been converted yet; they are still copies you make
yourself from `integrations/`. That is a distribution gap rather than a
capability one, but a fresh install is further from "it just works" for those
three than the other columns suggest.

**Tools are not uniform either.** Eight exist everywhere — `teleport_search`,
`_sessions`, `_turns`, `_peers`, `_ask`, `_reply`, `_live`, `_note`. Inbox, ack,
discovery and pairing are MCP-only; dsh has inbox and ack but not discovery or
pairing, and pi has none of the six. The skill names the `tp` CLI fallback
inline, so the capability is reachable everywhere, but the tool surface is not
the same and that is worth knowing before writing anything against it.

**The missing `/tp` is a platform absence, not an omission.** `/tp` is the
briefing a woken session reads before acting on an inbox that came from
somewhere else. codex has no slash-command mechanism to hang it on: a codex
plugin carries `skills/`, `hooks/`, `mcp`, `scripts`, `assets` and `apps`, and
nothing else. dsh declines it for a better reason — dsh commands run "without
sending anything to the model", so a command handler would be silently
half-wired, and dsh's Agent has a native inbox with a wakeup flag that already
*is* `tp ask`'s semantics.

codex's `SessionStart` hook is in one respect *better* than the Claude Code
path: codex states the session id, so teleport is told it rather than inferring
it by walking up the process tree. It also cannot be made fully automatic, and
should not be — codex records trust against a hash of the exact hook definition,
so a human approves it with `/hooks` before it runs.

**dsh's ⚠️ is structural.** It is the one runtime whose turns arrive by being
*pushed* (`tp ingest`) rather than by teleport reading a transcript: dsh
compresses its sessions and expresses history as a fold that a naive reader
would render *superseded content as current*, so the read stays inside dsh where
`ctx.sessionQuery` already resolves it. The cost is that only sessions created
after the extension was installed exist at all — the same limit an API proxy
has. Backfill is impossible, not merely unimplemented.

All of them see each other: from a Pi session you can search what a Claude Code
session worked out, and the reverse.

Cross-machine federation today means **read**. Cross-machine **reach** — poking
a session on another Mac — is designed but not wired end to end. Don't expect it
from a fresh install.

## Install

```bash
git clone https://github.com/agentmessier-ai/teleport.git
cd teleport
./install/install.sh
```

It builds release binaries into `~/.local/bin`, registers `io.teleport.tpd` as a
LaunchAgent (runs on login, restarts on crash, owns the SQLite store at
`~/.teleport/teleport.db`), installs the runtime configs, wires Claude Code's
installs the Claude Code plugin through `claude plugin install` (hooks, `/tp`,
skill and the `tp mcp` server, all four), writes the teleport skill to
`~/.agents/skills` (where pi, codex and dsh all look for it), installs the Pi
extension if Pi is present, and builds the menu bar panel if a Swift toolchain
is available. Re-running is safe and is also how you pick up changes: a
directory-sourced marketplace is snapshotted at install time, so the script
refreshes it every run.

If it finds no `claude` on PATH it says so and skips that step; teleport still
works, but no Claude Code session can be woken until the plugin is installed.

Building from source is fast on Apple silicon (about 1m 13s cold) and slow on
Intel — the release profile uses fat LTO, and on a 2019 Intel MacBook Pro the
final link alone runs tens of minutes. Prebuilt binaries are the answer and are
not shipped yet; until then, expect that wait on an Intel Mac.

Requirements: macOS, a [Rust toolchain](https://rustup.rs), and `~/.claude` for
Claude Code sessions. Optional: Pi, tmux, or iTerm2 for the respective reach
paths.

**Linux** is not supported, and the reason is narrower than that sounds. The
workspace has no platform-specific code — no `cfg(target_os)`, no macOS-only
dependencies — and the suite passes on x86_64 Linux. What has never been run
there is the *reach* half: waking a session, injecting into a terminal, and a
daemon that assumes launchd rather than systemd. Search and indexing would
likely work. Nobody has checked, so the answer stays "no" rather than
"probably".

### What takes effect when

Getting this wrong looks exactly like a change that silently didn't happen:

| Changed | Takes effect |
|---|---|
| `tp` binary → CLI, and Pi (which re-execs `tp` per call) | immediately |
| `tp` binary → **Claude Code's MCP tools** | **a new Claude Code session** — the MCP server is a long-lived child spawned at session start, so it keeps the binary it was launched with. `/reload-plugins` does *not* restart it. |
| skills / `tp.md` / the Pi extension | `/reload-skills` in Claude Code, `/reload` in a running Pi session |
| `~/.teleport/runtimes.d/*.toml` | next `tp`/`tpd` invocation — read per run |
| `tpd` | `install.sh` restarts it |

Uninstall: `./install/install.sh --uninstall`. Binaries and `~/.teleport` data
are left for you to remove explicitly.

## Reading a conversation back

Three modes, because what you know going in differs:

| You know | Use |
|---|---|
| a phrase | `tp search "…"` → coordinates + excerpt (cheap) |
| nothing | `tp sessions --since 4h` → which sessions were active |
| a time | `tp turns --since 4h --folder …` → full text, no session id needed |
| a session | `tp turns <id>` → full text from the start |
| a cursor | `tp turns <id> --after-ts <ms>` → resume forward |

Both time bounds accept a duration (`4h`, `2d`), a local date (`2026-08-04`), a
wall clock (`2026-08-04T14:30`), or unix ms. `--until` is exclusive, so a day is
`--since 2026-08-04 --until 2026-08-05` and paging back terminates.

Reading by time reads **one** session — the most recent in the window. That is a
guess, so it is only made when something narrows it: give `--folder` (or a
session id), or `tp turns` lists the candidates and stops rather than answering
a different question than you asked. `tp sessions` is the cross-session view.

The two directions are not the same operation, and the difference is the whole
point:

```
--since 4h        a window. Drops the OLDEST when it overflows → keeps "just now"
                  page back with:  --until <earliest ts>

--after-ts <ms>   a cursor. Drops the NEWEST → keeps the beginning
                  page on with:    --after-ts <last ts>
```

A truncated read always says so and prints the command that continues it, in the
direction it actually read.

## The `tp` CLI

| Command | What it does |
|---|---|
| `tp search <query>` | Search sessions. `--since`/`--until`, `--folder`, `--include-thinking`, `--regex`, `--all` (query peers). |
| `tp sessions` | Sessions active in the window, most recent first. `--since`/`--until`, `--folder`. |
| `tp turns [session_id]` | Read turns. `--since`/`--until` for a window (session id optional), `--after-ts` to resume forward, `--include-thinking`. |
| `tp live` | Sessions running right now, reconciled by `tpd`'s active scan — authoritative over hook registrations. |
| `tp ask <session_id> <msg>` | Enqueue into a session's mailbox and wake it. Stamps a return address so the target can answer. `--no-wake` parks it. |
| `tp reply <msg_id> <msg>` | Answer a message from your inbox, addressed from the original so it can't be misrouted. |
| `tp inbox` | Drain *this* session's mailbox — what `/tp inbox` triggers. |
| `tp type <tty> <text>` | **Unsafe path.** Types raw text into a pane: no mailbox, no control string, no gate. CLI-only by design, never an MCP tool. |
| `tp id` | This machine's fingerprint — compare out of band when pairing. |
| `tp peers` / `tp pair` | Trust state; `pair request/list/approve/reject`. |
| `tp discover <host>` | Ask one host whether it runs a daemon (default port + a few neighbours). Read-only: answering is not trusting. |
| `tp index` | Build/refresh the optional inverted index. **Not required** — scan is the default. |
| `tp mcp` | Run the MCP server over stdio. |
| `tp register` / `tp unregister` | Live-session registry, driven by hooks/extension events. |

Every command reports coverage explicitly. A truncated or degraded scan is never
presented as an answer — because "no matches" and "I didn't finish looking" are
the same output otherwise, and only one of them means it never happened.

## From an agent

**Claude Code** gets MCP tools: `teleport_search`, `teleport_sessions`,
`teleport_turns`, `teleport_ask`, `teleport_reply`, `teleport_inbox`,
`teleport_live`, `teleport_peers`, `teleport_discover`, `teleport_pair_*`.
**Pi** gets the same set as registered tools. A test asserts the two surfaces
expose the same parameters, because they have drifted before.

A skill teaches each agent what "carry over the conversation from my other Mac"
means step by step — and what *not* to do (never `pair_approve` speculatively;
never silently drop a peer that didn't answer a `--all` search).

**Messaging is asynchronous with no completion callback.** `tp ask` returns as
soon as the message is queued and the target woken; the target's `tp reply` is
the only signal the sender ever gets. An agent that needs an answer should send
and end its turn — the reply arrives as a `/tp inbox` wake that resumes it —
rather than polling in a `sleep` loop.

### Sending code in a message

Agents mostly send each other prose *about* code, so a message body normally
contains backticks, `$` and `!`. In a double-quoted shell argument the shell
expands those **before teleport sees anything**, and the message is delivered in
full with a silently different body — the failure mode this project keeps
finding elsewhere, arriving through the shell instead.

It has happened here: a backtick-quoted phrase became a command substitution,
resolved to the empty string, and removed the subject of the sentence around it.
The send reported success, because from teleport's side it was one.

Use a quoted heredoc:

```bash
tp reply <id> "$(cat <<'EOF'
`backticks`, $VARS and !history all survive verbatim
EOF
)"
```

The quotes around `EOF` are the load-bearing part — a bare `<<EOF` still
expands. Single-quoting the whole argument also works, until the body contains
an apostrophe.

Agents calling the MCP or pi tools (`teleport_ask`, `teleport_reply`) pass the
body as a JSON string and are not affected; this is a shell-only hazard.

## Security model

The reach path is built on one hard rule:

> **Only a fixed control string is ever typed into another session's pane.**
> Real content stays in the mailbox, where the receiving agent reads it through
> a tool call it can reason about and refuse.

Agents on this machine may be running with `--dangerously-skip-permissions`. If
message *content* were typed into a pane, anything that could reach the pane
would have arbitrary execution with no gate. Because only the control string
crosses, the pane is not an injection surface at all: the worst a poke can do is
make a session **check its inbox**.

**The trust boundary is the mailbox, not the prompt.** Only loopback or a peer
you approved by hand — after comparing fingerprints out of band — can put
anything in it. Inside that boundary a message is a *task*: the receiving agent
does the work and replies, applying the same judgement about risk it would to a
request typed by its operator. It is not given a separate, stricter rule for
messages, because a prompt-level rule was never an enforceable one — Claude Code
and Pi can prompt it, not enforce it, and leaning on it would have been
security theatre in place of the boundary that actually holds.

The practical consequence, stated plainly: **pairing a machine grants it the
ability to make your agents do work.** Pair only machines you'd hand a keyboard.
If you want a stricter posture than that, the place to put it is the receiving
agent's own permission gate, which teleport does not bypass.

- **Identity** — ed25519 keypair generated at install, private key `0600` at
  `~/.teleport/key`. The device fingerprint is what you compare out of band.
- **Pairing** — Syncthing-style: explicit request, then a human `approve` on
  *both* sides after comparing fingerprints. Trust is never automatic.
- **Peer requests** — signed per RFC 9421 over TLS. Reads are timestamp-bound.
  There is **no network write route at all**: pairing approval happens only via
  `tp pair approve`, which writes to the database directly. The HTTP endpoint
  that used to accept it was removed — it had no legitimate caller, and any
  local process could use it to make a remote machine permanently trusted.
- **Redaction** — every result passes one mandatory scrub funnel: `Hit` is only
  constructible through it, so no backend can skip it. The *patterns* are
  best-effort (AWS keys, `sk-ant-*`, `ghp_*`, private keys, bearer tokens, …) —
  a secret matching none of them passes through. The structural guarantee is
  that scrubbing is never skipped, not that every secret shape is known.
- **Reach guards** — ≤1 wake per target per 10 s; a `MAX_DELIVER` cap parks an
  undrained inbox instead of waking a session forever.

## Architecture

```
                     ┌────────────────────────── tpd (LaunchAgent) ─────┐
  ~/.claude/… ──────▶│  watcher → adapter → redact → SQLite (WAL)       │
  ~/.pi/…     ──────▶│  registry · live_session · mailbox               │
                     │  net: axum HTTP · probe · pairing · peer fan-out │
                     └───────┬──────────────────────────────┬───────────┘
                             │ 127.0.0.1:47400              │ LAN :47400 (TLS + sigs)
                   ┌─────────┴────────┐             ┌───────┴───────┐
                   │ tp CLI · MCP ·   │             │  peer machine │
                   │ hooks · panel    │             └───────────────┘
                   └──────────────────┘
```

- **Two binaries.** `tpd` is the resident daemon — it owns the DB, the watchers,
  the socket, the injectors. `tp` is a stateless CLI + MCP server. Hooks fire
  dozens of times per session, so the CLI must never open or migrate SQLite.
- **SQLite, not a cluster.** There's no shared mutable state: every machine
  writes its own sessions and peers only read. A 2-node Raft is strictly worse
  than 1 node — quorum is 2, so one sleeping laptop takes the cluster down.
  Cross-machine search is fan-out read at query time, with no sync protocol.
- **Retrieval is a swappable strategy.** The default is an on-demand scan,
  sub-second for the scoped queries the API pushes you toward; the inverted
  index (`tp index`) is an opt-in accelerator. Both providers run the *same*
  conformance suite — an abstraction exercised through one implementation isn't
  a seam, and every divergence must be either fixed or declared in
  `Capabilities`.
- **Runtimes are configuration.** An adapter is a TOML file in
  `~/.teleport/runtimes.d/`: a config with a known id overrides the built-in,
  a new id adds a runtime, and neither needs a rebuild. Both shipped runtimes
  *are* configs — verified by a differential run over the real corpus at zero
  divergences across 792k lines. A format the config language can't express
  still drops to a Rust `Adapter` impl.
- **macOS reach has a hard constraint.** A bare LaunchAgent cannot hold the
  Automation TCC grant, so iTerm2 AppleScript injection only works from
  CLI-initiated paths. tmux is the only TCC-free injection path, and `tpd` stays
  TCC-free by design.

**Where the reasoning is.** In the code, deliberately. Comments here carry the
measurement or the incident that produced a decision rather than restating what
the line does — why the scan provider reports what it cannot read, why pi's
compaction boundary is anchored and Claude Code's is positional, why a runtime
descriptor is a TOML file and not a Rust impl. The dense ones are worth reading
before changing anything near them:

- `crates/tp-core/src/turn.rs` — the vocabulary every layer speaks
- `crates/tp-db/src/writer.rs`, `query.rs` — how a turn is stored and found
- `crates/tp-ingest/src/adapter/decl/` — the descriptor engine
- `crates/tp-search/src/{scan,index}.rs` — the two providers that must agree
- `install/runtimes.d/*.toml` — one file per runtime, with the evidence for
  every field it maps

## The menu bar panel

`panel/` is a small SwiftUI `LSUIElement` app: human-readable **aliases** for
live sessions (keyed on `cwd`, so they survive `/clear` and restart), one-click
poke and focus, daemon health, and a recent-poke log. A read-only viewer and
launcher — `tpd`'s scan stays the source of truth.

## Development

```bash
cargo build --release
cargo test --workspace     # the conformance suite runs against both backends
cargo clippy --workspace --all-targets

# prove the shipped runtime configs still match the built-in adapters,
# over every session file on this machine
cargo run --release -p tp-ingest --example differential
```

Crates, in dependency order — the DAG is one-directional and deliberate:

| crate | what it owns |
| --- | --- |
| `tp-core` | types, ids, the retrieval contract. No I/O. |
| `tp-db` | SQLite, migrations, queries, the reach repository |
| `tp-ingest` | per-runtime transcript adapters, redaction |
| `tp-search` | Retrieval over two providers: on-demand scan, and the index |
| `tp-reach` | mailboxes, resolving a session, waking one |
| `tp-watch` | the file watcher that ingests continuously |
| `tp-net` | HTTPS federation, host probing, pairing, RFC 9421 |
| `tp-app` | the operations both surfaces call — returns values, never prints |
| `tp` | the binary: CLI (`main.rs`) + MCP server (`mcp.rs`) + daemon (`tpd`) |

`tp-app` is the one worth knowing about: `main.rs` renders prose for a person
and `mcp.rs` serialises JSON for a model, and both call ONE operation that
decides nothing about presentation. Every divergence between those two files
has so far been a bug.

## Roadmap

- Cross-machine reach — designed in LLD §7–8; needs an `.app` + SMAppService +
  XPC path for daemon-initiated injection
- More runtimes: openclaw, hermes — the read side is a TOML descriptor now,
  and codex was the first one added that way
- Indexing tool *results*, where test failures and error output actually live —
  they're dropped today, and turning them on is a redaction decision first
- Ranked (bm25) results on the scan path; today only the index ranks

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) — short on ceremony, long on the two or
three conventions here that a well-meaning patch tends to get wrong.

Security issues go to [`SECURITY.md`](SECURITY.md), not to the issue tracker.

## License

MIT — see [`LICENSE`](LICENSE).

Claude Code, Pi and Codex are names of their respective projects. Teleport
reads their files; it is not affiliated with any of them.
