# Handoff — IronClaw operator-experience cleanups (banner stale text, browser reconnect)

*2026-05-19. Target: terminal Claude working in `~/projects/ironclaw`.
Invoke CLIC Sonnet from this directory. Two small findings on the same
operator surface — one focused commit per finding, both in IronClaw repo.*

## Goal

Close two operator-experience papercuts that surfaced during 2026-05-19
substrate verification. Each is small, both are independent of each other,
together they tighten the operator surface so IronClaw behaves like a
service the operator can trust rather than a process they manage by eye.

Same theme as the ZP-side operator-surface hygiene sweep (commits
40cdba9–3b3b52b earlier this session) — applied to IronClaw instead.

## Two findings

### Finding 1 — Boot banner shows stale auth-mode text

**Observed today:** after wiring the Genesis-signed gate envelope path
(Phase 3, commit pushed earlier this session), IronClaw's boot banner
still reads:

```
ironclaw v0.28.0
...
auth        bearer (env-set)
```

The kid registration log line above the banner correctly shows the new
envelope auth:

```
INFO ZP cognition-governance hook registered base_url=http://localhost:17010
kid=595f98b47864cfeffa589b21f83a8d68dcf44bd3cdc9e8566d607bd19b7bce58
```

But the banner display logic doesn't reflect runtime auth posture. The
operator reads "auth bearer (env-set)" and infers the substrate is still
on the legacy path, when in fact every gate request goes through the new
envelope.

**Fix shape:** banner should detect active auth mode at runtime and
display accordingly. Three states:

| Banner text | Condition |
|-------------|-----------|
| `auth zp-sig (envelope)` | Envelope verifier is registered with a kid (current modern path) |
| `auth bearer (env-set)` | `ZP_SESSION_TOKEN` is in env but no envelope verifier active (legacy path) |
| `auth bearer (env-unset)` | Neither — startup config error worth flagging |

The display logic should match the order of priority: if envelope is
active, that's the mode regardless of whether a bearer token is also
present (both can coexist during backwards-compat window).

### Finding 2 — Browser tabs don't reconnect after IronClaw restart

**Observed today:** when IronClaw is killed and relaunched (any reason —
port conflict, restart cycle, intentional bounce), the operator's open
browser tabs at `http://127.0.0.1:3000/...` hold stale state and don't
automatically reconnect. Symptoms:

- The chat surface appears frozen / unresponsive
- Sending a message produces silent failure or browser error
- Operator has to hard-refresh (Cmd+Shift+R) or close and reopen the tab
- Existing conversations show but no new responses arrive

The root cause is that the browser-side JS holds open SSE/WebSocket
connections to the old IronClaw PID. When the new IronClaw spawns, the
old connections are dropped at the server but the client hasn't been
told to reconnect.

**Fix shape:** client-side reconnect logic with bounded backoff. Standard
pattern:

1. SSE/WebSocket connection drops detected (server closes, network
   error, or heartbeat timeout)
2. Client logs the disconnect, displays a brief "Reconnecting..."
   indicator in the chat UI
3. Reconnect attempts with exponential backoff (1s, 2s, 4s, 8s, capped
   at 15s) — bounded to avoid runaway retries
4. On successful reconnect, indicator clears, conversation resumes
5. On persistent failure (e.g., 30s of failed reconnects), surface a
   user-actionable error: "Server unavailable. Refresh the page to retry."

The substrate-session cookie persists across IronClaw restarts (HMAC
key is in the vault, not in process memory), and the gateway token in
the URL is similarly persistent. So reconnect doesn't require
re-authentication — just re-establishing the live channels.

## Investigation surface

```sh
# Finding 1 — banner display logic
grep -rn "auth.*bearer\|env-set\|auth.*zp-sig\|auth.*envelope" \
   ~/projects/ironclaw/src \
   ~/projects/ironclaw/crates 2>/dev/null

# Locate the banner emission site (probably during startup, after channels init)
grep -rn "ironclaw v0\|ready in\|banner\|startup_banner" \
   ~/projects/ironclaw/src \
   ~/projects/ironclaw/crates 2>/dev/null

# Find how envelope-active state is currently detected
grep -rn "EnvelopeVerifier\|envelope_verifier\|kid=\|GateSigner" \
   ~/projects/ironclaw/crates 2>/dev/null

# Finding 2 — frontend SSE/WebSocket connection management
grep -rn "EventSource\|WebSocket\|sse\|reconnect" \
   ~/projects/ironclaw/src \
   ~/projects/ironclaw/web 2>/dev/null

find ~/projects/ironclaw -name "*.js" -o -name "*.ts" 2>/dev/null | \
   xargs grep -l "EventSource\|WebSocket" 2>/dev/null | head -10
```

Confirm before designing:

- The banner template (likely a Rust `println!` or formatted string in
  startup code) — change is localized to that template plus the runtime
  detection
- The frontend's existing connection-management code — does it already
  have any reconnect logic, or is it strictly one-shot connection
- Whether the chat UI has a place to show transient status indicators
  ("Reconnecting...") without disrupting the conversation flow

## Deliverable

Two commits, scoped independently:

**Commit 1: `chore(banner): reflect active auth mode at runtime`**

- Modify banner emission to detect envelope verifier state
- Three-state display logic per the table above
- Test: spin up IronClaw with envelope path → banner shows `auth
  zp-sig (envelope)`; spin up without envelope → banner shows
  `auth bearer (env-set)`

**Commit 2: `feat(web): client-side reconnect with backoff on connection loss`**

- Detect SSE/WebSocket disconnect on the frontend
- Reconnect with exponential backoff (1s/2s/4s/8s/15s cap)
- Display transient "Reconnecting..." indicator during retries
- Surface persistent failure after ~30s as "Server unavailable" with
  manual retry CTA
- Test: kill IronClaw mid-conversation, verify the chat UI shows
  reconnecting indicator, restart IronClaw, verify connection resumes
  without page refresh

Run the pre-commit safety script before each push.

## Acceptance criteria

After both commits:

1. **Banner accuracy** — `auth` line reflects actual runtime mode.
   Operator reading the banner sees the truth about what auth path is
   active.
2. **Restart resilience** — operator can `kill` IronClaw, restart it,
   and the existing Comet tab resumes within ~5 seconds without
   manual refresh. The substrate-session cookie and gateway token
   persist across restarts; client-side reconnect handles the live
   channel.
3. **Failure UX is clean** — if IronClaw is down for longer than the
   reconnect window, the operator sees a clear "Server unavailable"
   message with manual retry, not a frozen UI or cryptic browser
   error.

## Edge cases

- **Restart happens during an in-flight message** — the partially-sent
  message may be lost (server didn't acknowledge before restart). UX
  should make this visible: don't silently lose the message; surface
  "Your last message may not have been received — please re-send if
  needed."
- **Network actually down (not just IronClaw restart)** — backoff
  still works, persistent-failure UX still applies. No special
  handling needed; same as IronClaw down.
- **Multiple tabs open** — each tab independently reconnects. No
  cross-tab coordination needed.
- **Authentication expired during disconnect** — if substrate-session
  cookie has aged out, reconnect succeeds at WebSocket level but
  subsequent requests get 401. Treat as authentication-required state;
  fall through to existing 401 handling. Not a reconnect concern.

## Out of scope

- **Heartbeat / keepalive on the connection** — separate concern. The
  reconnect logic handles drops; keepalive would prevent some drops.
  Add later if it bites.
- **Reconnect coordination across browser tabs** — single-tab
  semantics is sufficient.
- **Server-side push notification when IronClaw is about to restart**
  — would be nice (operator sees "Server restarting...") but requires
  protocol support. Defer.
- **Conversation state recovery if the server lost messages** — the
  conversations are stored in IronClaw's libSQL DB, which persists
  across restart. Reconnect should re-fetch the conversation; if the
  in-flight message wasn't persisted, the operator re-sends.
- **Banner text for multi-tenant / federated auth modes** — out of
  scope; current scope is single-operator local + envelope vs bearer.

## Connection to existing principles

- **Operator-surface hygiene principle** (CLAUDE.md, captured this
  session) — every output line should be useful signal, genuine
  error, or suppressed. The banner is signal that's currently
  misleading. Fixing it restores legibility.
- **Principle 8 (one canonical path)** — the banner's display logic
  should derive from one source of truth (the runtime auth state),
  not from a compile-time default that drifts. Same shape as the
  config-reflects-today heuristic applied to startup display.
- **Today's envelope work** — the kid registration in the cognition-
  governance hook is the runtime signal. Banner detection should
  read from the same state.

## Refs

- 2026-05-19 verification session — both findings surfaced as
  operator papercuts during the envelope auth verification
- Operator-surface hygiene sweep brief
  (`~/projects/zeropoint/docs/handoffs/operator-surface-hygiene-investigation-2026-05.md`)
  — same theme on the ZP side
- Today's envelope work (Phase 3 IronClaw commit) — what should now
  drive the banner display
- IronClaw frontend SSE/WebSocket code (path to confirm during
  investigation)
- Today's startup logs showing the banner with stale text alongside
  the kid registration with correct envelope path
