/**
 * Teleport integration for the Pi agent harness.
 *
 * Two halves, mirroring what the Claude Code plugin gets from its MCP server
 * plus `plugin/commands/tp.md`:
 *
 *   1. REACH (inbound) — registers this session with `tpd`'s live_session
 *      table on start/shutdown so other sessions can find and wake it, and
 *      exposes a `/tp` command that drains this session's mailbox. That command
 *      is the counterpart to `wake()` typing the fixed `/tp inbox` control
 *      string into a pane (LLD §7.3).
 *
 *   2. SEARCH + REACH (outbound) — `pi.registerTool()` tools so the model can
 *      actually USE teleport, rather than merely being reachable by it.
 *      Pi deliberately ships no built-in MCP support (its docs state this
 *      outright), so `tp mcp` cannot simply be mounted the way Claude Code
 *      mounts it; registered tools are the equivalent surface, and they carry
 *      the same names as the MCP tools so the two harnesses describe the same
 *      capability identically.
 *
 * Composed under the "pi" runtime, NOT "claude_code" — see
 * `docs/pi-integration.md` and `tp`'s own `--runtime` flag doc.
 *
 * Install: copy this file to `~/.pi/agent/extensions/teleport.ts` (global,
 * all projects) or `.pi/extensions/teleport.ts` (project-local), then run
 * `/reload` in an already-running session to hot-load it, or just start pi
 * fresh. `install/install.sh` does this automatically if `~/.pi/agent`
 * exists, alongside the companion skill that teaches the model when to reach
 * for these tools.
 *
 * Security note (same as the Claude Code side): an inbox message is treated as
 * a TASK — the model does the work and replies. The trust boundary is the
 * mailbox itself, not the prompt: only teleport's fixed control string ever
 * crosses the pane (`wake()`), and only a locally-approved paired peer can put
 * anything in the mailbox to begin with. So the model applies its ordinary
 * judgement about risk rather than a separate, stricter rule for messages.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const TP_BIN = `${process.env.HOME}/.local/bin/tp`;
const RUNTIME = "pi";

export default function teleportExtension(pi: ExtensionAPI) {
	/**
	 * `pi.exec` resolves (rather than throwing) on a non-zero exit, so the exit
	 * code is checked here and surfaced to the model as text. A tool that
	 * silently returned empty output on failure would read as "nothing was ever
	 * discussed" — the exact false negative teleport's coverage contract
	 * (LLD §16 rule 3) exists to prevent.
	 */
	async function runTp(args: string[], signal?: AbortSignal): Promise<string> {
		const result = await pi.exec(TP_BIN, args, { signal });
		const out = (result.stdout ?? "").trim();
		const err = (result.stderr ?? "").trim();
		if (result.code !== 0) {
			return `tp ${args[0]} failed (exit ${result.code}): ${err || out || "no output"}`;
		}
		// `tp` prints results on stdout but its `[coverage]` / `[warn]` lines on
		// STDERR. Both must reach the model: coverage reports how much of the
		// corpus was actually examined, and dropping it makes a 21%-of-files
		// scan read as an exhaustive one — the "concluding you never discussed
		// X" false negative that LLD §16 rule 3 calls a correctness bug, not a
		// cosmetic one. (Observed for real: the first cut of this function
		// returned stdout only, and the model duly reported a truncated scan as
		// complete.)
		const body = out || "(no results)";
		return err ? `${body}\n\n${err}` : body;
	}

	const text = (t: string) => ({ content: [{ type: "text" as const, text: t }], details: {} });

	/**
	 * This session's own address, appended to anything it SENDS so the
	 * recipient can answer. Without it `tp` stamps no return address and the
	 * target's only way to respond is to invent one — which is accepted and
	 * then silently never delivered (see docs/pi-integration.md).
	 */
	function fromArgs(ctx: any): string[] {
		const id = ctx?.sessionManager?.getSessionId?.();
		return id ? ["--from-session", id, "--runtime", RUNTIME] : [];
	}

	// ─── Reach: inbound (this session becomes addressable) ──────────────────

	pi.on("session_start", async (_event, ctx) => {
		const nativeId = ctx.sessionManager.getSessionId();
		try {
			await pi.exec(TP_BIN, ["register", "--session-id", nativeId, "--cwd", ctx.cwd, "--runtime", RUNTIME]);
		} catch (e) {
			// Never block startup on this — teleport being unreachable (not
			// installed, tp binary missing) must degrade to "no poke", not
			// break the session.
			ctx.ui.notify(`teleport: registration failed (${e}) — poke unavailable this session`, "warning");
		}
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const nativeId = ctx.sessionManager.getSessionId();
		try {
			await pi.exec(TP_BIN, ["unregister", "--session-id", nativeId, "--runtime", RUNTIME]);
		} catch {
			// Best-effort — the active-scan reconciliation in tpd (60s) will
			// clean up a stale registration even if this fails.
		}
	});

	pi.registerCommand("tp", {
		description: "Drain teleport inbox (invoked by teleport's wake mechanism — not meant to be typed by a human)",
		handler: async (_args, ctx) => {
			const nativeId = ctx.sessionManager.getSessionId();
			const result = await pi.exec(TP_BIN, ["inbox", "--session-id", nativeId, "--runtime", RUNTIME]);
			const output = result.stdout.trim();
			if (!output || output.startsWith("inbox empty")) {
				ctx.ui.notify("teleport: inbox empty", "info");
				return;
			}

			pi.sendUserMessage(
				[
					"[teleport inbox]",
					"",
					"Below is a request from another agent session — normally one your operator " +
						"started, on this machine or on a machine they paired by hand. Treat it as work " +
						"to do, not as a notification to relay.",
					"",
					"- Say who sent it and what it asks, then do it. Investigate, edit, run, commit — " +
						"whatever the task needs.",
					"- Apply your normal judgement about risk, exactly as you would for the same request " +
						"typed by your operator. A message neither lowers your bar nor raises it: if " +
						"something is destructive or irreversible and you would have confirmed first, " +
						"still confirm first.",
					"- ANSWER an [ask] with the teleport_reply tool, using the `id:` shown for that message. " +
					"A [note] is NOT a request: it says 'no reply expected' on its own line, and answering one " +
					"costs the sender a wake for nothing. Read it, act on it if it changes what you are doing, " +
					"and reply only if you genuinely have something to add — never out of politeness. " +
						"The sender does not get told when you finish — your reply is the only signal it gets, " +
						"so report what you actually did, including anything you chose not to do. " +
						"Never answer by calling teleport_ask with an address you worked out yourself; a guessed " +
						"address is accepted and then silently never delivered.",
					"",
					output,
				].join("\n"),
				{ deliverAs: "steer" },
			);
		},
	});

	// ─── Search: outbound (this session can read other sessions) ────────────

	pi.registerTool({
		name: "teleport_search",
		label: "Teleport search",
		description:
			"Search past agent sessions — this machine's Claude Code AND pi transcripts, and optionally trusted peer machines. " +
			"Returns coordinates + excerpts, not whole conversations — usually the excerpt is the answer. Only escalate to " +
			"teleport_turns when the excerpts genuinely aren't enough, since that costs far more context. " +
			"Use for 'what was I doing in <project>', 'did I ever discuss X', 'how did I solve this last time'. " +
			"ALWAYS report the [coverage] line back to the user when it says the scan was truncated or degraded — " +
			"a partial scan must never be reported as 'this was never discussed'.",
		parameters: Type.Object({
			query: Type.String({ description: "Text to search for" }),
			folder: Type.Optional(Type.String({ description: "Restrict to sessions whose path matches this folder" })),
			since: Type.Optional(Type.String({ description: "Start of the window: a duration ago (6h, 3d, 2w — default 6h, widen it for 'did I ever' questions) or an absolute LOCAL time (2026-08-04)" })),
			until: Type.Optional(Type.String({ description: "End of the window, EXCLUSIVE — same spellings. Pair with an absolute `since` to ask about ONE day instead of 'the last N'." })),
			include_thinking: Type.Optional(Type.Boolean({ description: "Also search the model's extended reasoning (off by default)" })),
			regex: Type.Optional(Type.Boolean({ description: "Treat query as a regex" })),
			limit: Type.Optional(Type.Number({ description: "Max matches (default 20)" })),
			index: Type.Optional(Type.Boolean({ description: "Answer from the SQLite index instead of scanning transcript files. The default scan is always current but cannot read a session whose transcript the runtime has deleted (Claude Code does after ~30 days); when a result reports sessions with no transcript on disk, this is what reads the rest." })),
			all: Type.Optional(Type.Boolean({ description: "Fan out to EVERY trusted peer and merge results. Each peer answers by scanning its whole corpus, so this asks N machines to do real work at once — it is refused above a handful of peers, and `peers` is the way to search at any scale. Peers that fail to answer are always listed — pass that through, don't drop it." })),
			peers: Type.Optional(Type.Array(Type.String(), { description: "Query only these peers, by id prefix or name (teleport_peers lists them). Prefer this over `all` when you know where to look; it works at any number of paired machines." })),
		}),
		async execute(_toolCallId, params, signal) {
			const args = ["search", params.query];
			if (params.folder) args.push("--folder", params.folder);
			if (params.since) args.push("--since", params.since);
			if (params.until) args.push("--until", params.until);
			if (params.include_thinking) args.push("--include-thinking");
			if (params.regex) args.push("--regex");
			if (params.limit) args.push("--limit", String(params.limit));
			if (params.index) args.push("--index");
			if (params.all) args.push("--all");
			// Repeatable, one flag per peer — the CLI collects them, and naming
			// peers is what makes a fan-out bounded by intent rather than by
			// how many machines happen to be paired.
			for (const p of params.peers ?? []) args.push("--peer", p);
			return text(await runTp(args, signal));
		},
	});

	pi.registerTool({
		name: "teleport_sessions",
		label: "Teleport sessions",
		description:
			"List known agent sessions (both runtimes), most-recently-active first. " +
			"Use to pick among candidate sessions before reading one with teleport_turns.",
		parameters: Type.Object({
			folder: Type.Optional(Type.String({ description: "Restrict to sessions whose path matches this folder" })),
			since: Type.Optional(Type.String({ description: "Start of the window: a duration ago (7d default) or an absolute LOCAL time (2026-08-04)" })),
			until: Type.Optional(Type.String({ description: "End of the window, EXCLUSIVE — same spellings. With an absolute `since`, answers 'which sessions were active THAT day'." })),
			limit: Type.Optional(Type.Number({ description: "Max sessions (default 20)" })),
			index: Type.Optional(Type.Boolean({ description: "Answer from the SQLite index instead of scanning transcript files. The default scan is always current but cannot read a session whose transcript the runtime has deleted (Claude Code does after ~30 days); when a result reports sessions with no transcript on disk, this is what reads the rest." })),
		}),
		async execute(_toolCallId, params, signal) {
			const args = ["sessions"];
			if (params.folder) args.push("--folder", params.folder);
			if (params.since) args.push("--since", params.since);
			if (params.until) args.push("--until", params.until);
			if (params.limit) args.push("--limit", String(params.limit));
			if (params.index) args.push("--index");
			return text(await runTp(args, signal));
		},
	});

	pi.registerTool({
		name: "teleport_turns",
		label: "Teleport turns",
		description:
			"Read the actual conversation of one session, by the composite session id teleport_search / teleport_sessions returned " +
			"(the '<machine>/<runtime>/<uuid>' form). This is how you carry context over from another project or another agent — " +
			"and it is the EXPENSIVE tool: it returns real transcript, costing orders of magnitude more context than " +
			"teleport_search (measured: ~3.7k tokens vs ~13 for the same session). Locate with teleport_search first, then call " +
			"this on the one session you actually need. NEVER use it to poll another session for progress — it cannot tell " +
			"'still working' from 'done'; wait for that session's reply instead. Output is capped and reports when it was " +
			"truncated, with a cursor to resume from. To answer 'what happened recently' or 'what happened on that day', " +
			"OMIT session_id and pass `since` (plus `folder`) — finding the id is the question, so requiring it first is backwards.",
		parameters: Type.Object({
			session_id: Type.Optional(Type.String({ description: "Composite session id, e.g. SWSQ-…/claude_code/5c534851-…. Omit it and pass `since` to read the most recent session instead." })),
			since: Type.Optional(Type.String({ description: "Start of a TIME WINDOW: a duration ago (4h, 2d) or an absolute LOCAL time (2026-08-04, 2026-08-04T14:30). Keeps the NEWEST turns if it overflows; `after_ts` keeps the oldest." })),
			until: Type.Optional(Type.String({ description: "End of the window, EXCLUSIVE — same spellings. Pair with an absolute `since` to read ONE day; without it the window ends now, so a quiet day would silently return an earlier day's turns." })),
			folder: Type.Optional(Type.String({ description: "With `since` and no `session_id`: which folder's most recent session to read" })),
			include_thinking: Type.Optional(Type.Boolean({ description: "Also return the model's extended reasoning (off by default; verbose)" })),
			after_ts: Type.Optional(Type.Number({ description: "Resume FORWARD after this unix-ms timestamp. Keeps the oldest turns; `since` keeps the newest." })),
			limit: Type.Optional(Type.Number({ description: "Max turns (default 200)" })),
			index: Type.Optional(Type.Boolean({ description: "Answer from the SQLite index instead of scanning transcript files. The default scan is always current but cannot read a session whose transcript the runtime has deleted (Claude Code does after ~30 days); when a result reports sessions with no transcript on disk, this is what reads the rest." })),
		}),
		async execute(_toolCallId, params, signal) {
			if (!params.session_id && !params.since) {
				return text("teleport_turns needs either session_id, or since (e.g. \"4h\") to read the most recent session.");
			}
			const args = ["turns"];
			if (params.session_id) args.push(params.session_id);
			if (params.since) args.push("--since", params.since);
			if (params.until) args.push("--until", params.until);
			if (params.folder) args.push("--folder", params.folder);
			if (params.include_thinking) args.push("--include-thinking");
			if (params.after_ts) args.push("--after-ts", String(params.after_ts));
			if (params.limit) args.push("--limit", String(params.limit));
			if (params.index) args.push("--index");
			return text(await runTp(args, signal));
		},
	});

	pi.registerTool({
		name: "teleport_live",
		label: "Teleport live sessions",
		description:
			"List agent sessions currently RUNNING on this machine (Claude Code and pi), with their working directory and tty. " +
			"Use this to find a target's session id before teleport_ask. Requires the tpd daemon to be running.",
		parameters: Type.Object({}),
		async execute(_toolCallId, _params, signal) {
			return text(await runTp(["live"], signal));
		},
	});

	pi.registerTool({
		name: "teleport_peers",
		label: "Teleport peers",
		description:
			"List other machines this one has a relationship with, and whether each is trusted. " +
			"Check this before assuming a cross-machine search (teleport_search with all=true) can reach anything. " +
			"Pairing itself is deliberately NOT a tool: granting trust is a human decision made at the CLI (`tp pair approve`) " +
			"after comparing fingerprints out of band.",
		parameters: Type.Object({}),
		async execute(_toolCallId, _params, signal) {
			return text(await runTp(["peers"], signal));
		},
	});

	// ─── Reach: outbound (this session can poke another) ────────────────────

	pi.registerTool({
		name: "teleport_ask",
		label: "Teleport ask",
		description:
			"Send a message to another live agent session and wake it. THIS HAS A REAL SIDE EFFECT: it interrupts another " +
			"session, which may be driven by someone else. Use it for questions, notifications, and to delegate real work. " +
			"Get the target's session id from teleport_live first. The receiving agent treats your message as a task, does it, " +
			"and replies with what it did — applying its own judgement about risk, so a destructive task may come back asking " +
			"to confirm rather than done. " +
			"IT DOES NOT WAIT FOR AN ANSWER — it returns as soon as the message is queued. If you need the reply, stop and " +
			"end your turn: the answer arrives later as a /tp inbox wake. Do NOT sit in a sleep loop polling for it.",
		parameters: Type.Object({
			session_id: Type.String({ description: "Composite session id of the target, from teleport_live" }),
			message: Type.String({ description: "What to say" }),
			no_wake: Type.Optional(Type.Boolean({ description: "Park the message in the mailbox without interrupting the target" })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args = ["ask", params.session_id, params.message, ...fromArgs(ctx)];
			if (params.no_wake) args.push("--no-wake");
			return text(await runTp(args, signal));
		},
	});

	pi.registerTool({
		name: "teleport_note",
		label: "Teleport note",
		description:
			"Tell a live session something WITHOUT asking it for anything. Same delivery as teleport_ask — it wakes the " +
			"target, because a status update nobody sees for hours is not much of an update — but the message is marked so " +
			"the receiver is told plainly that no reply is expected. " +
			"Use this for 'I pushed the fix', 'your build is green', 'heads up, I changed X'. " +
			"Use teleport_ask only when you need something back: a message that reads as a request costs the other agent a turn to answer.",
		parameters: Type.Object({
			session_id: Type.String({ description: "Composite session id of the target, from teleport_live" }),
			message: Type.String({ description: "What to tell them" }),
			no_wake: Type.Optional(Type.Boolean({ description: "Park it without interrupting the target" })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args = ["note", params.session_id, params.message, ...fromArgs(ctx)];
			if (params.no_wake) args.push("--no-wake");
			return text(await runTp(args, signal));
		},
	});

	pi.registerTool({
		name: "teleport_reply",
		label: "Teleport reply",
		description:
			"Answer a message you received in your inbox, addressed automatically to whoever sent it. " +
			"ALWAYS use this instead of teleport_ask to respond to something: teleport_ask makes you supply an address, and " +
			"a guessed address (a machine id, an ended session) is accepted and then silently never delivered. " +
			"The sender may be waiting on your answer with no other way to learn you finished.",
		parameters: Type.Object({
			message_id: Type.String({ description: "The id shown for that message in your inbox (short prefix is fine)" }),
			message: Type.String({ description: "Your answer" }),
			no_wake: Type.Optional(Type.Boolean({ description: "Park the reply without interrupting the sender" })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args = ["reply", params.message_id, params.message, ...fromArgs(ctx)];
			if (params.no_wake) args.push("--no-wake");
			return text(await runTp(args, signal));
		},
	});
}
