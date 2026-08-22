# Security

## Reporting a vulnerability

Report privately, not as a public issue: open a
[GitHub security advisory](https://github.com/agentmessier-ai/teleport/security/advisories/new)
on this repository. That gives us a private thread and a CVE if one is
warranted.

Please include what you did, what happened, and what you expected — a
reproduction beats a description. If you have a patch, attach it; if you would
like credit in the advisory, say so.

There is no bounty. This is one person's tool published in the hope it is
useful, and the honest thing is to say that up front rather than imply a
programme that does not exist.

Expect a first reply within a week. If a report is confirmed, the fix and the
advisory go out together.

## What is in scope

Teleport runs a daemon (`tpd`) that listens on the LAN, reads every AI coding
session transcript on the machine, and can wake other agent sessions. Anything
that crosses one of those boundaries is in scope:

- reaching the HTTP surface **without** being an approved peer, or as a peer
  whose approval was never given on both machines;
- forging, replaying or stripping an RFC 9421 signature on a request or a
  response;
- getting transcript content out of a machine that did not agree to share it —
  including through the redaction funnel, though see the known limit below;
- putting anything into a session's mailbox from outside the trust boundary;
- getting anything other than the fixed control string typed into a session's
  pane (see below — this is the load-bearing rule);
- reading or writing `~/.teleport/` as another local user, or making `tpd` do
  it on your behalf;
- crashing `tpd` remotely: it is what keeps every session reachable, so a
  remote panic is a denial of service against the whole machine.

## What is not

These are documented properties, not oversights. If you think one of them is
wrong, that is a design discussion — open an issue, not an advisory.

**A paired machine can make your agents do work.** Pairing is the trust
boundary and it is deliberate: a message that arrives in a mailbox is a *task*,
and the receiving agent does it, applying the same judgement it would to a
request typed by its operator. Pair only machines you would hand a keyboard.

**Redaction patterns are best-effort.** The structural guarantee is that the
scrub funnel is never skipped — `Hit` is only constructible through it, so no
backend can bypass it. The guarantee is *not* that every secret shape is known.
A secret matching none of the patterns passes through. A report that "this
novel token format is not redacted" is a welcome patch to the pattern list, not
a vulnerability; a report that "here is a path that produces a `Hit` without
scrubbing" is a vulnerability.

**Local processes running as you can read your data.** Teleport stores
transcripts in `~/.teleport/teleport.db` under your own account. Anything
running as you can read it, exactly as it can read `~/.claude`. The database is
not an additional boundary and does not claim to be.

**The prompt is not a control.** Receiving agents are not given a stricter rule
for messages than for their operator's own requests. Claude Code and Pi can
prompt a model, not enforce anything on it, and a prompt-level rule presented
as a security boundary would be theatre. The enforceable boundary is who can
put something in the mailbox.

## The rule the reach path rests on

> Only a fixed control string is ever typed into another session's pane. Real
> content stays in the mailbox, where the receiving agent reads it through a
> tool call it can reason about and refuse.

Agents on this machine may be running with `--dangerously-skip-permissions`. If
message *content* were typed into a pane, anything that could reach the pane
would have arbitrary execution with no gate. Because only the control string
crosses, the pane is not an injection surface: the worst a poke can do is make
a session check its inbox.

A way to get anything else into a pane is the highest-severity report this
project can receive.

## Supported versions

The tip of `main` is the only supported version. Teleport is installed by
building from source (`install/install.sh`), there are no release branches, and
no crate is published to crates.io. A fix means a commit on `main`.

## Where the design is written down

- `README.md` — the security model section, in full
- `docs/LLD.md` — §8 identity and pairing, §7 reach
- `docs/rfc9421-migration-design.md` — request and response signing, and why
  responses are signed at all
