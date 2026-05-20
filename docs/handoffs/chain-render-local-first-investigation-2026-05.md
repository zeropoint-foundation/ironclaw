# Handoff — chain_render local-first

*2026-05-19. Target: terminal Claude working in `~/projects/ironclaw`.
Invoke CLIC Sonnet from this directory. Closes the functional gap that
turned today's envelope verification into an indirect test rather than a
direct one.*

## Goal

Make the `chain_render` tool work for local-machine operators without
requiring a foundation cookie. The tool currently conflates two distinct
data sources — the local audit chain (operator's own substrate history)
and the foundation chain (multi-operator federation data at
zeropointfoundation.org). Local operators should query their own chain
through the local ZP gate (envelope-signed, no cookie); explicit opt-in
for foundation queries when the operator wants cross-machine data.

This is **Principle 8 (one canonical path)** applied to the chain
abstraction: one tool name, two documented sources, the auth surface
varies by source. Defaults are operator-current-machine-first because
that's the common case.

## Evidence — today's discovery

During 2026-05-19 substrate verification, the chain_render tool returned:

```
Tool error: Tool chain_render execution failed: Execution failed:
Foundation session not present on this request — chain rendering
requires a zp_session cookie. Sign in at zeropointfoundation.org and
retry from app.zeropointfoundation.org.
```

The tool was queried in IronClaw at `http://127.0.0.1:3000/` — the
local-machine surface. The operator has the local audit chain in
`~/ZeroPoint/data/audit.db`, the operator has a Genesis key that signs
local gate envelopes (today's substrate auth-tier work just verified
this end-to-end), and the operator is asking for their own onboarding
chain. None of this requires foundation auth — yet chain_render
refused because it's hardwired to the foundation worker's data source.

The error message names a workaround that requires the operator to
leave their local workflow (sign in at zeropointfoundation.org, switch
to app.zeropointfoundation.org). The substrate is telling the operator
that the local development surface is unsupported for chain queries,
even though the local chain is the obvious thing to ask about when
operating locally.

## The local-first model

Two chains, two auth surfaces, two sources:

| Source | Data location | Auth | Use case |
|--------|--------------|------|----------|
| `local` (default) | `~/ZeroPoint/data/audit.db` | Genesis-signed envelope via local ZP gate at `localhost:17010` | Operator's own substrate history. The common case. |
| `foundation` (opt-in) | Foundation worker D1 at zeropointfoundation.org | `zp_session` HMAC cookie | Cross-machine / multi-operator federation chain |

Same tool, parameter selects source:

```yaml
# Tool descriptor (conceptual)
chain_render:
  input:
    source: { type: "string", enum: ["local", "foundation"], default: "local" }
    # ... other params
```

Local is the default because:

- The operator running IronClaw locally has the local chain available
- The local gate auth (envelope) is the modern, verified path
- The foundation auth requires cross-domain cookie ceremony that breaks
  during local development

Foundation source remains available for explicit cross-machine queries.

## Composition with existing principles

- **Principle 2 (identity is a key)** — the operator's Genesis key
  authorizes local chain queries via envelope. No cookie required when
  the operator's own machine and key are present.
- **Principle 3 (there is no center)** — local chain is local. Querying
  one's own substrate history shouldn't depend on a remote service.
- **Principle 8 (one canonical path)** — one `chain_render` tool name;
  the source parameter selects the data origin. Future readers learn
  one tool, with documented modes, rather than two tools that drift.
- **Today's envelope work** — chain_render's local path uses the
  envelope auth verified end-to-end this session. The fix composes
  natively with the auth-tier arc that just closed.

## Investigation surface

The chain_render tool lives in IronClaw. Locate the implementation:

```sh
# Tool registration and dispatch
grep -rn "chain_render\|Foundation session not present" \
   ~/projects/ironclaw/crates \
   ~/projects/ironclaw/src 2>/dev/null

# How the tool currently queries the foundation chain
grep -rn "zp_session\|zeropointfoundation\|foundation.chain\|chain.foundation" \
   ~/projects/ironclaw/crates \
   ~/projects/ironclaw/src 2>/dev/null

# How other IronClaw tools talk to the local ZP gate (the envelope-signed path)
grep -rn "ZpClient\|zp_gate\|gate_url\|17010" \
   ~/projects/ironclaw/crates \
   ~/projects/ironclaw/src 2>/dev/null
```

Confirm before designing:

- What format does the local audit chain provide for chain receipts?
  (Receipt schema is in `zp-receipt` crate; chain walking is in
  `zp-verify` and `zp-server`.)
- Does the local ZP gate already expose a chain-query endpoint, or
  does this work need to add one?
- Are there existing IronClaw tools that talk to the local gate via
  envelope-signed requests? (The cognition-governance hook does; tool
  implementations may or may not.)

If the local gate doesn't expose a chain endpoint, this work splits
across both repos:

- ZP repo: add chain-query endpoint to the local gate (returns chain
  receipts for envelope-authenticated caller)
- IronClaw repo: chain_render gains source parameter; local source
  calls the new endpoint via ZpClient envelope path; foundation source
  retains existing cookie-based behavior

If the endpoint already exists, the work is IronClaw-only.

## The structural answer

### chain_render tool body

```rust
// Pseudocode shape
async fn chain_render(input: ChainRenderInput, ctx: &ToolContext) -> Result<...> {
    let source = input.source.unwrap_or(Source::Local);

    match source {
        Source::Local => {
            // Use ZpClient (envelope-signed) to call local gate's chain endpoint
            let receipts = ctx.zp_client.fetch_chain(input.filter).await?;
            render(receipts)
        }
        Source::Foundation => {
            // Existing path: requires zp_session cookie
            let cookie = ctx.request_cookies.get("zp_session")
                .ok_or_else(|| chain_render_foundation_auth_error())?;
            let receipts = fetch_foundation_chain(cookie, input.filter).await?;
            render(receipts)
        }
    }
}
```

### Updated error messages

When local source is requested but the local gate is unreachable:

```
chain_render local source failed: ZP gate at localhost:17010 unreachable.
Start the gate with `zp serve` and retry.
```

When foundation source is requested but cookie is missing:

```
chain_render foundation source failed: zp_session cookie not present on
this request. Sign in at zeropointfoundation.org and access from
app.zeropointfoundation.org. Or use source=local to query your local
chain.
```

The current error message conflates the two and tells the local
operator to leave their workflow. The new messages name the actual
problem per source and point at the correct fix.

### Tool descriptor

Update the chain_render descriptor (likely in `crates/.../tool.rs` or
similar) to expose the source parameter with documented enum values
and default. This ensures Sage's model sees the parameter and can
select it appropriately.

## Edge cases worth treating

- **Operator asks for "my onboarding chain" with no source** — defaults
  to local. The common case is satisfied by default. No cookie
  required.
- **Operator asks for foundation chain on a machine with no
  zeropointfoundation.org cookie** — foundation source fails with the
  cookie-not-present error, plus the explicit suggestion to use
  source=local for local data.
- **Operator on app.zeropointfoundation.org (foundation surface with
  cookie present) asks for "my chain" with default source=local** —
  works. The local source doesn't require the cookie; it requires
  envelope auth, which is present in any IronClaw deployment.
- **Operator on app.zeropointfoundation.org explicitly asks for
  source=foundation** — works via existing cookie path.
- **Local chain query returns 0 receipts** (no audit history yet) —
  render an explicit empty state, not an error. "No chain receipts
  yet — your onboarding may still be in progress."
- **Local chain query returns receipts that don't verify** (signature
  corruption, hash chain break) — surface the integrity failure as a
  receipt, not a chain_render failure. Composes with `zp verify` for
  diagnostic depth.

## Deliverable

`docs/handoffs/chain-render-local-first-design-2026-05.md` covering:

1. **Current state map** — exact chain_render implementation today,
   where the foundation-cookie check lives, what endpoint it calls
2. **Local chain endpoint design** — whether the local ZP gate already
   exposes one (if yes, what it returns; if no, what to add)
3. **chain_render tool signature change** — exact source parameter
   schema, default value, tool descriptor update
4. **Error message updates** — replacement text for both sources
5. **Tests** — local source query succeeds without cookie, foundation
   source query still requires cookie, default-source behavior, both
   error messages
6. **Implementation files** — concrete list across IronClaw (and ZP
   if endpoint addition needed)
7. **Migration / backwards compat** — if any Sage prompts or operator
   workflows depend on the current foundation-only behavior, name them.
   Most workflows that say "show me my chain" will improve silently.
   Foundation queries that didn't specify source will need explicit
   source=foundation after the change — name the affected callers if
   any exist.

## Acceptance criteria

After the design lands (and ultimately the implementation):

1. **`chain_render` invoked at `http://127.0.0.1:3000` with no source
   parameter returns the operator's local chain successfully.** No
   cookie required.
2. **Error messages are operator-actionable**, naming the actual
   problem per source and the correct fix.
3. **Foundation source still works** for cross-machine queries with a
   valid cookie.
4. **Tool descriptor exposes the source parameter** so Sage can pick it
   when intent is explicit.
5. **Composes with the envelope path** — local source uses the
   envelope-signed call to localhost:17010, the same auth surface
   verified end-to-end this session.

## Out of scope

- **Federation of local and foundation chains** (e.g., merged view).
  Each source is queried independently; merging is future work.
- **Foundation chain auth improvements**. The existing cookie path
  stays as-is; this brief is about not requiring it for local queries.
- **Sage prompt updates**. If Sage's reasoning prompt mentions
  chain_render in ways that assumed foundation-only behavior, those
  updates are tracked separately. The tool descriptor change is the
  load-bearing part; prompt updates are polish.
- **Audit chain GUI** (a `zp chain` subcommand or web view). Different
  surface, related but separate scope.
- **Cross-operator local chains** (i.e., one operator querying
  another's local chain on the same machine). Out of scope; the
  multi-tenant identity work (#99/#120) handles that surface.

## Connection to existing work

- **Today's genesis-signed gate envelope (Phases 1+2+3)** is the
  load-bearing auth primitive that makes local source possible. Without
  envelope auth, the local gate would need its own cookie ceremony or
  bearer token. The envelope verified end-to-end this session is
  exactly what local chain_render needs.
- **The audit chain itself** is already chain-walking machinery in
  `zp-verify` and the local gate. Likely the chain-query endpoint
  already exists (the local gate already responds to envelope-signed
  requests for governance decisions); chain_render just needs to call
  the right endpoint.
- **#149 (IronClaw auth re-prompt loop)** is adjacent — same theme of
  IronClaw's local surface not having the auth context it needs.
  Different surface (chat-level), same architectural shape.
- **#138 (Remove Cloudflare Access from app.zeropointfoundation.org)**
  closed the gate-tier auth at the production surface. This brief
  closes the chain-tier auth at the local surface. Symmetric work.

## Refs

- Today's verification session — local envelope path succeeded for
  `time` tool; chain_render failed with foundation-cookie error
- Genesis-signed gate envelope work (commits 5cd7e21 + sibling in
  IronClaw — Phases 1+2+3 of `docs/handoffs/genesis-signed-gate-requests-design-2026-05.md`)
- `docs/skill-audit-2026-05.md` — earlier audit of IronClaw skills
- Audit chain implementation in `zp-verify`, `zp-server` (ZP repo)
- chain_render tool registration in IronClaw (specific path to be
  confirmed during investigation)
