/**
 * teleport ↔ DeepSeek Harness (dsh)
 *
 * A Cordis plugin, not an external transcript reader, and the reason is not
 * that dsh happens to support plugins. Reading dsh's files from outside means
 * reimplementing four mechanisms it already has — zstd frame decoding, the
 * packed-chunk codec, a three-level path whose session id is a DIRECTORY name,
 * and role-asymmetric field paths — plus a fifth that is a real algorithm
 * rather than a mapping: the *surface fold* that decides which events are
 * current, which are shadowed, and how positional replacements resolve.
 * Compaction is expressed through that fold. A reader that skips it does not
 * get a degraded view; it presents superseded content as current.
 *
 * Inside dsh, `ctx.sessionQuery` hands all of that over, and it stays dsh's
 * problem when dsh changes it. See docs/dsh-integration.md.
 *
 * SCOPE: this file is the REACH half — registration, presence, and receiving a
 * wake. The read half (pushing turns to `tp ingest`) is deliberately separate;
 * its weakness is that it only ever sees sessions created after the plugin was
 * installed, the same structural limit as an API proxy.
 *
 * Install: add to `~/.dsh/profiles/<name>/cordis.patch.yml`
 *
 *     - insert:
 *         - id: teleport
 *           name: '@teleport/dsh'
 */

import { spawn } from 'node:child_process'
import { homedir } from 'node:os'
import { join } from 'node:path'
import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Context } from '@deepseek-ai/cordis'
import { defineTool } from '@deepseek-ai/dsh-tools'

export const name = 'teleport'

/**
 * Only what this plugin cannot work without.
 *
 * `tools` is required, not optional, and for a reason specific to dsh: its
 * default `workspace-write` sandbox allows writes to exactly
 * `[workspaceRoot, /tmp, os.tmpdir()]`, so a session shelling out to `tp reply`
 * gets "attempt to write a readonly database" — `~/.teleport` is outside all
 * three. Verified live. These tools execute in the HOST process, outside that
 * sandbox, which is what makes replying possible at all. `webServer` is deliberately NOT
 * here — a CLI-only composition has none, and requiring it would stop this
 * plugin loading at all there.
 *
 * But omitting it from `inject` also makes it unreadable: Cordis throws
 * "cannot get property X without inject" on the very access, so a
 * `typeof ctx.webServer` guard is not a guard, it is the crash. (Verified the
 * hard way — that error took down a running dsh.) The correct shape for an
 * OPTIONAL service is a nested `ctx.inject([...], cb)` scope, which runs only
 * when the service is actually mounted; dsh uses it for `typert` in
 * `core/agent/src/index.ts`.
 */
export const inject = ['agents', 'tools', 'sessionQuery']

const RUNTIME = 'dsh'
const TP = process.env.TP_BIN ?? join(homedir(), '.local', 'bin', 'tp')

/**
 * teleport's PRESENCE_TTL is 90s and a row is marked stale — not evicted — when
 * it lapses. Beating at a third of that means two consecutive failures still
 * leave the session healthy.
 */
const HEARTBEAT_MS = 30_000

/**
 * Run `tp` and return stdout+stderr merged.
 *
 * Merging is not cosmetic. `tp` prints `[coverage]` and `[warn]` lines to
 * stderr — including "this scan was truncated" — and pi's extension originally
 * returned stdout only, which reported a partial scan to the model as a
 * complete one. That is the exact false negative teleport's coverage contract
 * exists to prevent, reintroduced at the integration layer.
 */
function tp(args: string[], signal?: AbortSignal): Promise<string> {
  return new Promise((resolve) => {
    const child = spawn(TP, args, { signal })
    let out = ''
    child.stdout?.on('data', (d) => (out += d))
    child.stderr?.on('data', (d) => (out += d))
    // Never reject: teleport being absent, unreadable, or slow must degrade to
    // "no teleport" and never break the dsh session hosting us.
    child.on('error', (e) => resolve(`teleport unavailable: ${e.message}`))
    child.on('close', () => resolve(out.trim()))
  })
}

/** Fire-and-forget: a failed register must not break session startup. */
function tpDetached(args: string[]): void {
  void tp(args)
}

export function apply(ctx: Context): void {
  // ── Presence ───────────────────────────────────────────────────────────────
  // `declared`, because dsh cannot be found by teleport's process scan: the web
  // profile's sessions live in a browser with no tty, and one host process
  // serves many of them. Without this the scan would prune these rows within
  // one interval — which is exactly what happened to pi before teleport learned
  // that the scan may only prune what it can see.
  // Resolved when the wake route is actually mounted; until then registration
  // declares no channel and teleport parks messages in the mailbox — correct,
  // and better than declaring a channel that would 404.
  let deliver: string | undefined = process.env.TELEPORT_DSH_WAKE
    ? `exec:${process.env.TELEPORT_DSH_WAKE}`
    : undefined

  ctx.on('session/created', (session) => {
    tpDetached([
      'register',
      '--session-id', session.id,
      '--runtime', RUNTIME,
      '--cwd', session.header.cwd ?? process.cwd(),
      '--presence', 'declared',
      // Our OWN pid, not the one teleport would infer. Its fallback walks up
      // from the spawned `tp` to the nearest registered ancestor, which finds
      // whatever launched dsh — observed live: a host started from a Claude
      // Code session got every dsh session registered under the CLAUDE pid,
      // which then made that session's own `tp ask` look like it came from dsh.
      '--pid', String(process.pid),
      ...(deliver ? ['--deliver', deliver] : []),
    ])
  })

  ctx.on('session/disposed', (session) => {
    tpDetached(['unregister', '--session-id', session.id, '--runtime', RUNTIME])
  })

  // Renew every live session. teleport marks a lapsed row stale rather than
  // deleting it, so a missed beat costs a wake, not the registration.
  const timer = setInterval(() => {
    for (const agent of ctx.agents.list()) {
      tpDetached(['heartbeat', '--session-id', agent.id, '--runtime', RUNTIME])
    }
  }, HEARTBEAT_MS)
  ctx.on('dispose', () => clearInterval(timer))

  // ── Delivery ───────────────────────────────────────────────────────────────
  // teleport POSTs a fixed control string here; the body never carries message
  // content (LLD §7.3). We drain our own inbox and put the result in front of
  // the model — the one step that cannot live in teleport, because only code
  // inside dsh can do it.
  //
  // `agent.send(msg, 'next-turn', true)` rather than a `/tp` slash command: dsh
  // commands execute "without sending anything to the model", so a command
  // handler that only returned text would be silently half-wired. dsh's Agent
  // has a native inbox with a wakeup flag, which is `tp ask`'s semantics
  // expressed natively.
  registerTools(ctx)
  pushTurns(ctx)

  // OPTIONAL, like `webServer`: a composition without it leaves `titleReader`
  // undefined and pushes carry no title, which is the honest degradation —
  // teleport still derives its own fallback from the turns.
  ctx.inject(['sessionTitle', 'sessions'], (titled: Context) => {
    titleReader = (sessionId: string) => {
      // `sessions.get(id): Session | undefined` rather than
      // `agents.get(id)?.session`: both reach a Session, but the agent route is
      // only defined for a LIVE agent, and a push can outlive one.
      //
      // `sessionTitle.get(session)` is the SYNCHRONOUS read of the already-folded
      // snapshot. `readTitle` is the async sibling; awaiting it here would hold
      // up the turn being shipped for something that is already in memory.
      const session = titled.sessions.get(sessionId)
      return session ? titled.sessionTitle.get(session) : undefined
    }
    titled.effect(() => () => {
      titleReader = undefined
    })
  })

  ctx.inject(['webServer'], (web: Context) => {
    deliver = `http://127.0.0.1:${web.webServer.port}/teleport/wake`
    const dispose = web.webServer.register({
      kind: 'exact',
      path: '/teleport/wake',
      handler: async (req: IncomingMessage, res: ServerResponse) => {
        try {
          const sessionId = await readSessionId(req)
          if (!sessionId) {
            res.statusCode = 400
            return res.end('missing session_id')
          }
          // teleport addresses sessions by its COMPOSITE id
          // (`<machine>/<runtime>/<native>`); dsh knows only the native part.
          // Verified the hard way: the first live wake was delivered, answered
          // "no such live session", and left the message unread — the composite
          // id simply is not a key in `ctx.agents`.
          const nativeId = sessionId.split('/').pop() ?? sessionId
          const agent = ctx.agents.get(nativeId as never)
          if (!agent) {
            // Not an error: teleport may still hold a registration for a
            // session this host has since disposed. Answer 200 so the wake is
            // not retried against a session that no longer exists here.
            res.statusCode = 200
            return res.end('no such live session')
          }
          // `tp inbox` accepts either form, but pass the native id for symmetry
          // with registration, which also passes native and lets `tp` compose.
          const drained = await tp(['inbox', '--session-id', nativeId, '--runtime', RUNTIME])
          if (drained && !drained.startsWith('inbox empty')) {
            agent.send(userMessage(`${FRAMING}\n\n${drained}`), 'next-turn', true)
          }
          res.statusCode = 200
          res.end('ok')
        } catch (e) {
          res.statusCode = 500
          res.end(String(e))
        }
      },
    })
    web.on('dispose', dispose)
  })
}

/**
 * The searchable half: push this session's turns into teleport's index.
 *
 * Turns are derived from `sessionQuery.readSurface()`, never from raw events.
 * That is the whole reason this integration is a plugin: the surface fold has
 * already run, so only `current` nodes are sent and compaction is observed as
 * its replacement rather than as the superseded content it replaced. A reader
 * outside dsh would have to reimplement that fold to be correct.
 *
 * Pushing happens on `turn/end` — not per chunk, which would be wasteful and
 * would send text the fold may later replace.
 *
 * The limit, stated rather than discovered: this only ever sees sessions that
 * were live while the plugin was installed. It supplements teleport's disk
 * reading; it does not replace it.
 */
function pushTurns(ctx: Context): void {
  // Per-session high-water mark, so each push carries only what is new.
  // `capturedThroughSeq` is `readSurface`'s own cursor, so this stays aligned
  // with what dsh considers captured rather than with a count we maintain.
  const pushedThrough = new Map<string, number>()

  // `turn/end` is an event TYPE in the log, not a subscribable ctx event —
  // verified the hard way: subscribing to it silently never fired. The
  // post-commit feed is `session/event`, which carries the appended event, so
  // the turn boundary is a filter here rather than a separate subscription.
  ctx.on('session/event', (session: { id: string }, event: { type?: string }) => {
    if (event?.type !== 'turn/end') return
    void (async () => {
      try {
        const snap = await ctx.sessionQuery.readSurface(session.id)
        const from = pushedThrough.get(session.id) ?? -1
        const fresh = snap.events.filter((e: SurfaceEvent) => e.seq > from)
        const turns = fresh.map(toNormalizedTurn).filter((t): t is NormalizedTurn => t !== undefined)
        if (turns.length === 0) return

        await tpStdin(
          [
            'ingest',
            '--session-id', session.id,
            '--runtime', RUNTIME,
            ...(snap.session?.cwd ? ['--cwd', String(snap.session.cwd)] : []),
            ...titleArgs(session.id),
          ],
          JSON.stringify(turns),
        )
        if (snap.capturedThroughSeq !== null) pushedThrough.set(session.id, snap.capturedThroughSeq)
      } catch {
        // Never surface an ingest failure into the session. teleport being
        // absent or slow must cost searchability, never the user's turn.
      }
    })()
  })

  ctx.on('session/disposed', (session: { id: string }) => {
    pushedThrough.delete(session.id)
  })
}

interface SurfaceEvent {
  seq: number
  type: string
  time?: number
  data?: Record<string, unknown>
}

interface NormalizedTurn {
  role: 'user' | 'assistant'
  ts: number | null
  text: string
  thinking: string
  tool_calls: { name: string; input_digest: string | null }[]
  tokens_in: number | null
  tokens_out: number | null
  prov: {
    uuid: string | null
    parent_uuid: string | null
    model: string | null
    cache_read_tokens: number | null
    cache_creation_tokens: number | null
  }
}

/**
 * One surface event → teleport's normalized shape, or `undefined` for anything
 * that is not a conversational turn.
 *
 * Only the TERMINAL message events are mapped. `assistant/chunk` and the packed
 * chunk rows are streaming deltas that `assistant/message` supersedes — it even
 * carries `sourceEventSeqs` naming the ones it assembled — so mapping them too
 * would duplicate every assistant turn.
 *
 * The field paths are asymmetric between roles and that is not a typo:
 * `user/message` keeps content at `data.content`, `assistant/message` at
 * `data.message.content`. Verified against a real session log.
 */
function toNormalizedTurn(e: SurfaceEvent): NormalizedTurn | undefined {
  const d = e.data ?? {}
  const ts = typeof e.time === 'number' ? e.time : null

  if (e.type === 'user/message') {
    const { text, thinking, tools } = collect(d.content)
    return {
      role: 'user', ts, text, thinking, tool_calls: tools,
      tokens_in: null, tokens_out: null,
      prov: { uuid: str(d.id), parent_uuid: null, model: null, cache_read_tokens: null, cache_creation_tokens: null },
    }
  }
  if (e.type === 'assistant/message') {
    const m = (d.message ?? {}) as Record<string, unknown>
    const usage = (d.usage ?? {}) as Record<string, unknown>
    const source = (m.source ?? {}) as Record<string, unknown>
    const { text, thinking, tools } = collect(m.content)
    return {
      role: 'assistant', ts, text, thinking, tool_calls: tools,
      tokens_in: num(usage.inputTokens), tokens_out: num(usage.outputTokens),
      prov: {
        uuid: str(m.id),
        parent_uuid: null,
        model: str(source.model),
        cache_read_tokens: num(usage.cacheReadTokens),
        cache_creation_tokens: null,
      },
    }
  }
  return undefined
}

/** dsh content blocks → teleport's (text, thinking, tool_calls) triple. */
function collect(content: unknown): { text: string; thinking: string; tools: { name: string; input_digest: string | null }[] } {
  const texts: string[] = []
  const thinks: string[] = []
  const tools: { name: string; input_digest: string | null }[] = []
  if (typeof content === 'string') texts.push(content)
  else if (Array.isArray(content)) {
    for (const b of content as Record<string, unknown>[]) {
      if (b?.type === 'text' && typeof b.text === 'string') texts.push(b.text)
      // dsh calls it `reasoning`; teleport's field is `thinking`.
      else if (b?.type === 'reasoning' && typeof b.text === 'string') thinks.push(b.text)
      else if (b?.type === 'tool-call') {
        tools.push({
          name: String(b.name ?? '?'),
          // A DIGEST, never the raw input: tool payloads are large and
          // frequently carry secrets (LLD §4).
          input_digest: typeof b.arguments === 'string' ? b.arguments.slice(0, 80) : null,
        })
      }
    }
  }
  return { text: texts.join(''), thinking: thinks.join(''), tools }
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)
const num = (v: unknown): number | null => (typeof v === 'number' ? v : null)

/**
 * The title dsh STATES, as `--title` plus its source.
 *
 * dsh's own model is three-way — `SessionTitleSource` is
 * `{kind:'user'} | {kind:'provider',…} | {kind:'fallback'}` — which lines up
 * exactly with teleport's three title columns, arrived at independently. Only
 * the first two are forwarded:
 *
 *   user     → --title-source user   (an explicit rename; pins the title)
 *   provider → --title-source ai     (a model or registered provider wrote it)
 *   fallback → NOT SENT
 *
 * `fallback` is dsh's own derivation from the opening messages, which is the
 * same CATEGORY as teleport's `title_derived`. Forwarding it would put a value
 * teleport cannot re-derive into the column teleport owns, so a re-index would
 * silently disagree with a push. Skipping it costs nothing: teleport derives its
 * own fallback from the turns being pushed in the same call.
 *
 * `sessionTitle` is an OPTIONAL service — a CLI-only composition may not mount
 * it — and Cordis throws on reading an uninjected property, so it is reached
 * through the nested-inject scope rather than a `typeof` guard (the same hazard
 * documented for `webServer` above: the guard IS the crash).
 */
function titleArgs(sessionId: string): string[] {
  const snapshot = titleReader?.(sessionId)
  if (!snapshot) return []
  const kind = snapshot.source?.kind
  if (kind !== 'user' && kind !== 'provider') return []
  const title = String(snapshot.title ?? '').trim()
  if (!title) return []
  return ['--title', title, '--title-source', kind === 'user' ? 'user' : 'ai']
}

/**
 * Set from inside the `sessionTitle` inject scope, so a composition without the
 * service leaves it undefined and `titleArgs` degrades to sending no title.
 */
let titleReader: ((sessionId: string) => { title?: string; source?: { kind?: string } } | undefined) | undefined

/** `tp` with a body on stdin — `ingest` reads its turns that way. */
function tpStdin(args: string[], body: string): Promise<string> {
  return new Promise((resolve) => {
    const child = spawn(TP, args)
    let out = ''
    child.stdout?.on('data', (d) => (out += d))
    child.stderr?.on('data', (d) => (out += d))
    child.on('error', (e) => resolve(`teleport unavailable: ${e.message}`))
    child.on('close', () => resolve(out.trim()))
    // The stdin stream needs its OWN error handler. `child.on('error')` covers
    // spawn failure, not a write to a pipe the child already closed — and an
    // unhandled 'error' on a socket is fatal to the whole Node process. This
    // took down a running dsh: an older `tp` rejected `ingest` as an unknown
    // subcommand and exited before the body was written, so end(body) hit EPIPE.
    // A child that dies early must cost this one push, nothing more.
    child.stdin?.on('error', () => resolve('teleport: delivery pipe closed early'))
    child.stdin?.end(body)
  })
}

/**
 * The outbound half: teleport as tools the model can call.
 *
 * Every one shells out to `tp`, so the logic stays in one shared binary and
 * this file remains a translation layer. Names and parameters match the MCP
 * tool set exactly — `crates/tp/tests/pi_tool_parity.rs` guards that for pi and
 * should be extended to cover dsh, because the same drift has happened twice.
 */
function registerTools(ctx: Context): void {
  const text = (value: unknown) => [{ type: 'text', text: String(value) }]
  // `required` must be `true` or ABSENT — dsh's schema compiler rejects
  // `required: false` outright ("must be true when present"), which is stricter
  // than JSON Schema and caught at plugin load rather than at call time.
  const str = (description: string, required = false) =>
    (required ? { type: 'string', required: true, description } : { type: 'string', description })

  ctx.tools.register(defineTool({
    name: 'teleport_search',
    description:
      'Find WHERE something was discussed across agent sessions on this machine — Claude Code, pi, and dsh alike — and optionally on trusted peer machines. ' +
      'Returns coordinates and excerpts, not whole conversations; feed a hit into teleport_turns when the excerpt is not enough. ' +
      'ALWAYS pass the [coverage] line back to the user when it reports a truncated or degraded scan: a partial scan is not proof that something was never discussed.',
    parameters: {
      query: str('Text to search for', true),
      folder: str('Restrict to sessions whose path matches this folder'),
      since: str('Start of the window: a duration ago (6h, 3d, 2w — default 6h) or an absolute local date (2026-08-04)'),
      until: str('End of the window, EXCLUSIVE — same spellings. Pair with an absolute `since` to ask about ONE day.'),
      include_thinking: { type: 'boolean', description: "Also search the model's extended reasoning (off by default)" },
      limit: { type: 'number', description: 'Max matches (default 20)' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return tp([
        'search', String(a.query),
        ...optStr('--folder', a.folder), ...optStr('--since', a.since), ...optStr('--until', a.until),
        ...(a.include_thinking ? ['--include-thinking'] : []),
        ...optStr('--limit', a.limit),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_sessions',
    description: 'List agent sessions active in a time window, most recent first — use it to pick one before teleport_turns.',
    parameters: {
      folder: str('Restrict to sessions whose path matches this folder'),
      since: str('Start of the window (default 7d), a duration or an absolute local date'),
      until: str('End of the window, EXCLUSIVE'),
      limit: { type: 'number', description: 'Max sessions (default 20)' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return tp(['sessions', ...optStr('--folder', a.folder), ...optStr('--since', a.since),
        ...optStr('--until', a.until), ...optStr('--limit', a.limit)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_turns',
    description:
      'Read the actual conversation of a session. This is the EXPENSIVE tool — it returns real transcript, orders of magnitude more context than teleport_search. ' +
      'To answer "what happened recently" or "what happened on that day", OMIT session_id and pass `since` with `folder`: finding the id IS the question. ' +
      'Never use it to poll another session for progress — it cannot tell "still working" from "done"; wait for that session to reply.',
    parameters: {
      session_id: str('Composite session id from teleport_search / teleport_sessions. Omit it and pass `since` to read the most recent session instead.'),
      since: str('Start of a TIME WINDOW: a duration ago (4h, 2d) or an absolute local time (2026-08-04). Keeps the NEWEST turns if it overflows.'),
      until: str('End of the window, EXCLUSIVE. Pair with an absolute `since` to read ONE day.'),
      folder: str('With `since` and no `session_id`: which folder\'s most recent session to read'),
      include_thinking: { type: 'boolean', description: "Also return the model's extended reasoning" },
      limit: { type: 'number', description: 'Max turns (default 200)' },
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      if (!a.session_id && !a.since) return 'teleport_turns needs either session_id, or since (e.g. "4h") to read the most recent session.'
      return tp(['turns', ...(a.session_id ? [String(a.session_id)] : []),
        ...optStr('--since', a.since), ...optStr('--until', a.until), ...optStr('--folder', a.folder),
        ...(a.include_thinking ? ['--include-thinking'] : []), ...optStr('--limit', a.limit)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_live',
    description: 'List agent sessions running RIGHT NOW on this machine, with their working directory. Use it to find a target before teleport_ask.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute() {
      return tp(['live'])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_ask',
    description:
      'Send a message to another live agent session and wake it. THIS HAS A REAL SIDE EFFECT: it interrupts another session, which may be driven by someone else. ' +
      'Use it for questions, notifications, and to delegate real work — the receiving agent treats the message as a task, does it, and replies with what it did. ' +
      'IT DOES NOT WAIT. It returns as soon as the message is queued; the answer arrives later as a wake. Do NOT sit in a sleep loop polling for it.',
    parameters: {
      session_id: str('Composite session id of the target, from teleport_live', true),
      message: str('What to say', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return tp(['ask', String(a.session_id), String(a.message), ...identity(exec)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_note',
    description:
      'Tell another live session something WITHOUT asking it for anything. Same delivery as teleport_ask — it wakes the target, because a status update ' +
      'nobody sees for hours is not much of an update — but the message is marked so the receiver is told plainly that no reply is expected. ' +
      "Use it for 'I pushed the fix', 'your build is green', 'heads up, I changed X'. Use teleport_ask only when you need something back: " +
      'a message that reads as a request costs the other agent a turn to answer.',
    parameters: {
      session_id: str('Composite session id of the target, from teleport_live', true),
      message: str('What to tell them', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return tp(['note', String(a.session_id), String(a.message), ...identity(exec)])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_peers',
    description:
      'List the other machines this one has paired with, and their trust state. Use it before searching another machine: teleport_search with `peers` ' +
      'set queries only the ones you name, while `all` fans out to every trusted peer and each answers by scanning its whole corpus.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute() {
      return tp(['peers'])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_reply',
    description:
      'Answer a message from your inbox, using the `id:` shown for it. ALWAYS prefer this over teleport_ask when responding: the address comes from the original ' +
      'message so it cannot be misrouted, and a reply is the ONLY signal the sender ever gets — including when you have finished the work, not just when asked a question.',
    parameters: {
      message_id: str('The `id:` printed for that message', true),
      message: str('Your answer — report what you actually did, including anything you chose not to do', true),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args, exec) {
      const a = args as Record<string, unknown>
      return tp([
        'reply', String(a.message_id), String(a.message),
        // Identify ourselves explicitly. Without this `tp` infers the sender by
        // walking up from the spawned process, which lands on whatever launched
        // dsh — so a reply was stamped as coming from THAT session instead.
        // Contract R-capable: "a runtime that knows its own session id should
        // pass --from-session; never rely on inference."
        ...identity(exec),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_inbox',
    description:
      'Drain THIS session\'s own mailbox. Normally teleport wakes you automatically; call this if the user asks whether anyone messaged you. ' +
      'Draining marks a message READ, not ACKED — read means shown, ack means you confirm you finished acting on it (teleport_ack). ' +
      'Set pending to see messages that were shown but never acked: the recovery view if a previous drain got interrupted before you finished ' +
      'acting on everything in it. Read-only — it does not drain or mark anything.',
    parameters: {
      session_id: str('This session\'s native id'),
      pending: { type: 'boolean', description: 'Show delivered-but-unacked messages instead of draining new ones. Read-only.' },
      history_since: str('Show ACKED messages from this window instead of draining new ones — a duration ("4h", "2d") or an absolute local time. Read-only.'),
    },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return tp([
        'inbox', ...optStr('--session-id', a.session_id), '--runtime', RUNTIME,
        ...(a.pending ? ['--pending'] : []),
        ...(a.history_since ? ['--history', '--since', String(a.history_since)] : []),
      ])
    },
  }))

  ctx.tools.register(defineTool({
    name: 'teleport_ack',
    description:
      'Confirm you finished acting on a message from your inbox — NOT the same as teleport_inbox showing it to you. Call this only after you ' +
      'have actually done what the message asked (or decided a note needs no action). An unacked message stays visible forever via ' +
      'teleport_inbox with pending set, so if you get interrupted mid-batch, the next drain can recover exactly what you left undone.',
    parameters: { message_id: str('The message_id from your inbox (short prefix is fine)', true) },
    output: { schema: { type: 'string' }, render: (_a, v) => text(v) },
    async execute(args) {
      const a = args as Record<string, unknown>
      return tp(['ack', String(a.message_id)])
    },
  }))
}

/**
 * `--from-session <native> --runtime dsh`, from the execution context.
 *
 * The tool runs inside the session that called it, so its id is known here and
 * must be stated: teleport's fallback is to infer the sender from the process
 * tree, which for a dsh host launched from another agent session resolves to
 * THAT session. Observed live — replies were stamped as coming from the Claude
 * Code session that started dsh.
 */
function identity(exec: unknown): string[] {
  // `ToolExecutionInput.agent?.id` — the Agent whose turn invoked this tool.
  // Optional in the type, so absence degrades to teleport's inference rather
  // than to a broken call.
  const sid = (exec as { agent?: { id?: string } })?.agent?.id
  return sid ? ['--from-session', String(sid), '--runtime', RUNTIME] : []
}

/** `--flag value` when present, nothing when not. */
function optStr(flag: string, v: unknown): string[] {
  return v === undefined || v === null || v === '' ? [] : [flag, String(v)]
}

async function readSessionId(req: IncomingMessage): Promise<string | undefined> {
  const chunks: Buffer[] = []
  for await (const c of req) chunks.push(c as Buffer)
  try {
    const body = JSON.parse(Buffer.concat(chunks).toString('utf8')) as { session_id?: string }
    return body.session_id
  } catch {
    return undefined
  }
}

/** Shape a plain-text user message for `agent.send`. */
function userMessage(text: string): never {
  return {
    id: `teleport-${Date.now()}`,
    role: 'user',
    content: [{ type: 'text', text }],
    source: { kind: 'user' },
  } as never
}

/**
 * Copied verbatim from `plugin/commands/tp.md`, which is canonical (contract
 * R4). Two hand-maintained copies have drifted before, which is why
 * `crates/tp/tests/pi_tool_parity.rs` exists — extend it to cover dsh rather
 * than paraphrasing this.
 */
const FRAMING = [
  '[teleport inbox]',
  '',
  'Below is a request from another agent session — normally one your operator',
  'started, on this machine or on a machine they paired by hand. Treat it as work',
  'to do, not as a notification to relay.',
  '',
  '- Say who sent it and what it asks, then do it. Investigate, edit, run, commit —',
  '  whatever the task needs.',
  '- Apply your normal judgement about risk, exactly as you would for the same',
  '  request typed by your operator. A message neither lowers your bar nor raises',
  '  it: if something is destructive or irreversible and you would have confirmed',
  '  first, still confirm first.',
  '- ANSWER an [ask] with `tp reply <id> "..."`, using the `id:` shown for it.',
  '- A [note] is NOT a request: it says "no reply expected" on its own line, and',
  '  answering one costs the sender a wake for nothing. Read it, act on it if it',
  '  changes what you are doing, and reply only if you genuinely have something',
  '  to add — never out of politeness.',
  '  The sender does not get told when you finish — your reply is the only signal',
  '  it gets, so report what you actually did, including anything you chose not to',
  '  do. Never answer by calling `tp ask` with an address you worked out yourself;',
  '  a guessed address is accepted and then silently never delivered.',
].join('\n')
