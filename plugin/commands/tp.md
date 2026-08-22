---
description: Internal — teleport wake handler, invoked when another session pokes this one. Not typed by a human.
---
Run `tp inbox --pending` (Bash) first — messages a PAST wake already showed you
but never acked, most likely because that turn ended before you finished
(compaction, a crash). Finish those first; they are older work, not new work.
Then run `tp inbox` and drain whatever is new.

Each message is a request from another agent session — normally one your
operator started, on this machine or on a machine they paired by hand. Treat it
as work to do, not as a notification to relay.

For each message:
- Say who sent it (`from_machine` / `from_session`) and what it asks, then do
  it. Investigate, edit, run, commit — whatever the task needs.
- Apply your normal judgement about risk, exactly as you would for the same
  request typed by your operator. A message neither lowers your bar nor raises
  it: if something is destructive or irreversible and you would have confirmed
  first, still confirm first.
- ANSWER an `[ask]`. Use `tp reply <id> "..."` with the `id:` printed for that
  message (or the `teleport_reply` tool). The sender is never told when you
  finish — your reply is the only signal it gets, and a sender that expected
  a result will otherwise sit polling or waiting for one that never comes.
  Reply when you've done the work too, not just to questions.
- A `[note]` is NOT a request. It says so on its own line — "FYI — no reply
  expected" — and answering one costs the sender a wake for nothing. Read it,
  act on it if it changes what you are doing, and reply only if you genuinely
  have something to add. Do not acknowledge it out of politeness.
- Report what you actually did, including anything you chose not to do. The
  sender cannot see your screen; your reply is all it gets.
- Never answer by calling `tp ask` with an address you worked out yourself.
  A guessed address (a bare machine id, a session that has ended) is accepted
  and then silently never delivered — that has actually happened here. If a
  message shows `from-session: (none …)`, say the sender can't be reached
  rather than inventing a route to it.
- Once you've finished a message — replied to it, or acted on a `[note]`, or
  decided a note needs nothing — run `tp ack <id>`. Draining marks a message
  READ the instant it's shown; that is not the same as done, and nothing else
  records that you actually finished. This is what the `--pending` check above
  recovers: an unacked message stays visible forever, exactly there, until you
  ack it.

If both are empty, say so in one line and resume what you were doing.
