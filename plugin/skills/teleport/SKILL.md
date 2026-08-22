---
name: teleport
description: Carry a recent agent conversation from another folder — or another Mac on the LAN — into this session, find WHERE something was ever discussed across sessions/machines, message a live session, or pair with a new machine. Use when the user says "what was I doing in <folder/project>", "carry over / bring over the conversation from <folder>", "continue what I started in <other repo>", "pull my session from <project>", "teleport the conversation from my other Mac", "did I ever <do X>", "find where I talked about <X>", "message my other session about X", "pair with my other Mac", or otherwise wants context from — or wants to reach — an agent session that happened somewhere else.
allowed-tools: mcp__plugin_teleport_teleport__teleport_search mcp__plugin_teleport_teleport__teleport_sessions mcp__plugin_teleport_teleport__teleport_turns mcp__plugin_teleport_teleport__teleport_peers mcp__plugin_teleport_teleport__teleport_discover mcp__plugin_teleport_teleport__teleport_pair_request mcp__plugin_teleport_teleport__teleport_pair_list mcp__plugin_teleport_teleport__teleport_pair_approve mcp__plugin_teleport_teleport__teleport_pair_reject mcp__plugin_teleport_teleport__teleport_ask mcp__plugin_teleport_teleport__teleport_reply mcp__plugin_teleport_teleport__teleport_inbox mcp__plugin_teleport_teleport__teleport_ack mcp__teleport__teleport_search mcp__teleport__teleport_sessions mcp__teleport__teleport_turns mcp__teleport__teleport_peers mcp__teleport__teleport_discover mcp__teleport__teleport_pair_request mcp__teleport__teleport_pair_list mcp__teleport__teleport_pair_approve mcp__teleport__teleport_pair_reject mcp__teleport__teleport_ask mcp__teleport__teleport_reply mcp__teleport__teleport_inbox mcp__teleport__teleport_ack mcp__plugin_teleport_teleport__teleport_live mcp__teleport__teleport_live mcp__plugin_teleport_teleport__teleport_note mcp__teleport__teleport_note
---

# Teleport

Cross-session, cross-agent and cross-machine memory, backed by the `tp`/`tpd` binaries.

**What is searchable:** every Claude Code, pi, codex and dsh session on this machine. That is the
point — from one agent you can read what another worked out. Cross-machine search additionally
needs a peer that has been discovered AND manually approved; never assume a peer is reachable
without checking `teleport_peers` first.

**If a tool named below does not exist in this runtime, run the `tp` CLI equivalent instead.**
Harnesses expose different subsets: eight tools (`teleport_search`, `_sessions`, `_turns`,
`_peers`, `_ask`, `_reply`, `_live`, `_note`) exist everywhere; inbox, ack, discovery and pairing
are tools under MCP and plain commands elsewhere. The capability is the same either way — only the
calling convention changes, and `tp <verb> --help` is authoritative for the arguments.

## "What was I doing in X" / "find where I talked about X"

1. `teleport_search` with `query` (and `folder` if the user named a project). Omit `folder` to
   search everywhere. This returns coordinates + excerpts, not full conversations.
2. Check `coverage` in the result — `truncated: true` or a non-null `degraded` means the search
   didn't cover everything. Say so; never report "never happened" off a degraded scan.
3. Usually the excerpt IS the answer — report it and stop. `teleport_turns` is the expensive
   tool (~3.7k tokens against ~13 for the same session via search), so escalate to it only when
   the excerpts genuinely aren't enough; then call it with that hit's `session_id`.
   Use `teleport_sessions` first if you need to pick among several candidate sessions instead of
   jumping straight to the most recent hit. Both take `since`/`until` — narrow the window before
   widening the search, since an unscoped scan is the slow path and reports itself as degraded.

## "What was I doing this afternoon" / "what happened on <date>"

A question about a TIME, not about a phrase. Don't search for it — read the
window directly with `teleport_turns`:

- `since` takes a duration (`"4h"`, `"2d"`) **or an absolute local time**
  (`"2026-08-04"`, `"2026-08-04T14:30"`). `until` is the exclusive end, so one
  day is `since: "2026-08-04", until: "2026-08-05"`.
- Omit `session_id` — finding it IS the question. Pass `folder` instead.
- Without `folder`, a window that matches several sessions is refused with the
  candidates listed, because reading one arbitrary session would answer a
  different question. Either narrow it or show the user the list and ask.
- Use `teleport_sessions` with the same bounds for the cross-session view:
  *which* sessions were active then, without pulling any transcript.

`since` and `after_ts` page opposite ways and are not interchangeable: a window
keeps the NEWEST turns when it overflows (page back with `before_ts`), a cursor
keeps the oldest (page on with `after_ts`). A truncated response says which
cursor to use next — follow it rather than constructing your own.

## "Pull the conversation from my other Mac" / cross-machine search

1. `teleport_peers` — confirm the target machine is `trusted`. If it isn't there at all, run
   `teleport_discover` to find it on the LAN, then see Pairing below.
2. `teleport_search` with `all: true` — this fans out to every trusted peer and merges results.
   The response's `peers.failed` and `peers.no_address` arrays list any peer that didn't answer;
   surface those explicitly rather than silently dropping them, especially before telling the user
   "not found anywhere."

## Pairing a new machine

Trust is never automatic — a human must confirm identity out of band.

The pairing and discovery tools exist only under MCP. Elsewhere the same four steps are
`tp discover <host>`, `tp pair request <addr>`, `tp pair list`, `tp pair approve <device-id>` —
identical semantics, including that approve is the only step that grants anything.

1. `teleport_discover` to find candidates on the LAN (or get `host:port` from the user directly).
2. `teleport_pair_request` with that `addr`. This does NOT grant trust — it just introduces both
   sides and returns a `device_id`.
3. Show the user the `device_id` and ask them to confirm it matches what's shown on the other
   machine (they may need to run `tp id` there, or ask their agent there to do the same pairing
   flow — pairing is two-sided, both machines need `teleport_pair_approve`).
4. Only after that human confirmation, call `teleport_pair_approve` with the `device_id`. Do not
   approve on the user's say-so alone if they haven't actually compared the id — that's the entire
   point of the step.

## Messaging a live session

- `teleport_ask` with a target `session_id` and `message` enqueues a message into that session's
  mailbox and wakes it if it's in a tmux pane or an iTerm2 session — otherwise it's picked up next
  time that session runs `/tp inbox` itself. The target does the work and replies — it applies
  its own judgement about risk, the same as it would for a request typed by its operator, so a
  destructive or irreversible task may still come back asking to confirm.
- `teleport_inbox` drains the CURRENT session's own mailbox — call it if the user asks "did anyone
  message me" or you notice a `/tp inbox` control string. Where that tool is absent, `tp inbox`
  does the same thing.
- `teleport_reply` answers a message from your inbox, using its `message_id`. Always prefer it over
  `teleport_ask` for responding — see below.

### ask does not wait; reply is the only completion signal

`teleport_ask` returns as soon as the message is queued. There is no callback, no blocking mode,
and no "is it done yet" query. When you need the other session's answer, send and then **end your
turn**, saying you're waiting — the reply arrives later as a `/tp inbox` wake that resumes you.
Don't `sleep`-poll `teleport_turns` on the target: it is not slow but it is expensive — it dumps
real transcript into your context (~3.7k tokens against ~13 for the same session via search) — and
it still can't distinguish "working" from "done".

Symmetrically, when YOU receive a task: reply when you've finished it, not just when asked a
question. The sender has no other way to know.

Answer with `teleport_reply` and the `message_id` from your inbox — never by calling `teleport_ask`
with an address you constructed. A guessed address (a bare machine id, an ended session) is
accepted and then **silently never delivered**; that has really happened here. A message whose
`repliable` is `false` genuinely has no return path — say so instead of inventing one.

### drained is not the same as finished — ack when you're actually done

`teleport_inbox` marks a message **read** the instant it hands it to you. That is not the same as
having acted on it — it is only "shown". Once you've actually finished one (replied to it, acted on
a note, or decided a note needs nothing), call `teleport_ack` with its `message_id` — or run
`tp ack <id>` where that tool is absent. A message that
was shown but never acked is not lost: it stays visible forever via `teleport_inbox` with
`pending: true` (`tp inbox --pending`), which is exactly the recovery path for a turn that ended mid-batch — compaction, a
crash, anything that stops you before you finish everything you drained. Check `pending: true`
**before** draining new messages, so older, unfinished work gets picked back up ahead of new work
rather than sitting invisible next to it.

## What NOT to do

- Don't call `teleport_pair_approve` speculatively "to see what happens" — it grants a peer read
  access to every session on this machine.
- Don't retry a failed peer silently and report success from partial results; `teleport_search`
  with `all: true` already tells you who didn't answer — pass that through.
