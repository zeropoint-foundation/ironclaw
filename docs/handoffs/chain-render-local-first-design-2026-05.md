# chain_render Local-First Design

*2026-05-19. Closes the functional gap discovered during today's substrate verification session:
`chain_render` invoked at `http://127.0.0.1:3000` returns a foundation-cookie error when the
operator is asking for their own local chain. This document is the design; implementation
tracking is separate.*

---

## 1. Current State Map

### Tool implementation

`src/tools/builtin/chain_render.rs` — 333 lines.

**Auth check (L176–183):** First thing `execute()` does is demand `ctx.substrate_session`:

```rust
let session = ctx.substrate_session.as_ref().ok_or_else(|| {
    ToolError::ExecutionFailed(
        "Foundation session not present on this request — chain rendering \
         requires a zp_session cookie. Sign in at zeropointfoundation.org \
         and retry from app.zeropointfoundation.org."
            .to_string(),
    )
})?;
```

`SubstrateSessionInfo` (`src/context/state.rs:138`) is set only when the gateway middleware
validates a `zp_session` HMAC cookie issued by `zeropointfoundation.org`. It is absent for
every request that arrived via envelope auth, bearer token, or CLI — i.e., for every local
operator.

**Foundation chain call (L191–204):** GET to `$FOUNDATION_BASE_URL/api/operator/me/chain?claim_pattern=…`
with `Authorization: Bearer <session.session_token>`. Returns `{"receipts": [...], "count": N}`.

**Voice anchor (L239–250):** GET to `$FOUNDATION_BASE_URL/narratives/foundation-director-onboarding.yaml`
— a public static asset, no auth required.

**Constants:**

```rust
const DEFAULT_FOUNDATION_BASE_URL: &str = "https://zeropointfoundation.org";
const CHAIN_PATH: &str = "/api/operator/me/chain";
const NARRATIVE_PATH: &str = "/narratives/foundation-director-onboarding.yaml";
```

**Parameters schema (L152–163):** Only `claim_pattern` (glob string, default `"onboard:*"`).
No `source` parameter exists.

### Where the foundation-cookie check lives

`src/context/state.rs:228–230`: `JobContext.substrate_session: Option<SubstrateSessionInfo>`.
Set at job creation by the dispatcher (`src/agent/dispatcher.rs:57`) and thread ops
(`src/agent/thread_ops.rs:1544`) via `with_substrate_session(message.substrate_session.clone())`.
The session originates from the web channel middleware at `src/channels/web/platform/auth.rs`.

### What the foundation chain endpoint returns

Consumer-side receipt shape (per `docs/RECEIPT-CHAIN-VIZ-2026-05.md` §Data shape):

```json
{
  "id": "rcp-7c4ed102",
  "prev_id": "rcp-a3f9c821",
  "operator_id": "ken",
  "claim": "onboard:identity:generated",
  "metadata": {"fingerprint": "47d7...7b92"},
  "signature": "ed25519:base64url:...",
  "signature_status": "verified",
  "created_at": "2026-05-13T14:23:15Z"
}
```

The `chain_render` parsing code reads `receipt["claim"]` and `receipt["metadata"]` directly —
it treats the array as opaque JSON for the LLM narration and only destructures the `claim` field
for the deterministic closer logic.

### ZpClient and JobContext

`src/zp/client.rs` — the `ZpClient` holds a Genesis-derived Ed25519 signer and exposes:
- `gate_tool_call(tool_name, args_hash, thread_id, run_id)` → `GateDecision`
- `observe(user_input, assistant_response, chain_parent_receipt_id)` → `()`
- `signed_request(method, path_and_query, body)` → `reqwest::Response`

**`ZpClient` is not available to tools.** In `src/app.rs:1180–1184`:

```rust
match crate::zp::ZpClient::new(&zp_cfg, signer) {
    Ok(client) => {
        let hook = Arc::new(crate::zp::ZpHook::new(Arc::new(client)));
        hooks.register(hook).await;
```

The client is moved into the hook immediately. `JobContext` has no `zp_client` field.

### Local ZP gate existing endpoints

`GET /api/v1/audit/chain-head` — returns `{"latest_hash": "...", "chain_algorithm": "Blake3"}`.
Only the hash; no receipts.

`POST /api/v1/capabilities/verify-chain` — integrity check, no receipt query.

**No endpoint returns chain receipts filtered by claim pattern.** The local gate has no
analogue of `/api/operator/me/chain`.

### Local audit store

`~/ZeroPoint/data/audit.db` — SQLite, managed by `zp-audit::AuditStore`.
Methods relevant to a chain query:

| Method | Filter | Return type |
|--------|--------|-------------|
| `export_chain(limit)` | None | `Vec<AuditEntry>` |
| `query_by_claim_type(claim_type, limit)` | Exact `receipt_type` string | `Vec<AuditEntry>` |

**No glob/prefix pattern matching exists.** The `query_by_claim_type` query is:

```sql
WHERE json_extract(receipt, '$.receipt_type') = ?1
```

`AuditEntry.receipt` is `Option<Receipt>` where `Receipt.receipt_type` is a `ReceiptType` enum
(e.g., `"execution"`, `"observation_claim"`, `"policy_claim"`). Onboarding receipts from the
local wizard (`zp serve`) use the same enum types — the `onboard:*` colon-delimited claim
strings live in `receipt.action.operation` or `receipt.extensions`, not in `receipt_type`.

---

## 2. Local Chain Endpoint Design

**Work is split across two repos.** The local gate must expose a new endpoint before IronClaw
can call it.

### New endpoint: `GET /api/v1/operator/chain`

**File:** `crates/zp-server/src/lib.rs` (route registration) +
`crates/zp-server/src/handlers.rs` or a new `crates/zp-server/src/chain.rs`

**Auth:** Genesis envelope (`Authorization: ZP-Sig …`) — the same `EnvelopeMiddleware` that
guards `/api/v1/gate/tool-call`. No `zp_session` cookie required.

**Query params:**

| Param | Default | Semantics |
|-------|---------|-----------|
| `claim_pattern` | `onboard:*` | Glob or prefix filter on claim string (e.g. `onboard:*`, `tool:*`, `*`) |
| `limit` | `100` | Cap on receipt count returned |
| `order` | `asc` | `"asc"` (oldest first) or `"desc"` (newest first) |

**Claim extraction from `AuditEntry`:**

Each `AuditEntry` carries `receipt: Option<Receipt>`. The claim string is derived in this order:
1. `receipt.extensions["zp:claim"]` — if the onboard wizard writes a custom extension key
2. `receipt.action.as_ref().map(|a| &a.operation)` — the operation field on the `Action` type
3. Fallback: `receipt.receipt_type.as_str()` — the enum's wire name

Route handler implementation sketch:

```rust
async fn operator_chain_handler(
    State(state): State<AppState>,
    Query(params): Query<ChainQueryParams>,
) -> Result<Json<ChainQueryResponse>, StatusCode> {
    let store = state.0.audit_store.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let entries = store.export_chain(params.limit.unwrap_or(100))?;

    let receipts: Vec<ChainReceiptView> = entries
        .into_iter()
        .filter_map(|e| {
            let receipt = e.receipt?;
            let claim = extract_claim_string(&receipt)?;
            if !glob_matches(&params.claim_pattern.as_deref().unwrap_or("onboard:*"), &claim) {
                return None;
            }
            Some(ChainReceiptView {
                id: receipt.id.clone(),
                prev_id: receipt.parent_receipt_id.clone(),
                claim,
                metadata: extract_metadata(&receipt),
                signature_status: derive_signature_status(&receipt),
                created_at: receipt.created_at,
            })
        })
        .collect();

    Ok(Json(ChainQueryResponse {
        count: receipts.len(),
        receipts,
    }))
}
```

**Response shape** (matches foundation consumer-side contract):

```json
{
  "count": 7,
  "receipts": [
    {
      "id": "rcp-7c4ed102",
      "prev_id": "rcp-a3f9c821",
      "claim": "onboard:identity:generated",
      "metadata": {"fingerprint": "47d7...7b92"},
      "signature_status": "verified",
      "created_at": "2026-05-13T14:23:15Z"
    }
  ]
}
```

**Note on claim extraction:** Before implementing, confirm how the onboard wizard writes claim
strings to the local chain. Run:

```sql
SELECT json_extract(receipt, '$.action'), json_extract(receipt, '$.extensions')
FROM audit_entries
WHERE json_extract(receipt, '$.receipt_type') IS NOT NULL
LIMIT 10;
```

against `~/ZeroPoint/data/audit.db` to see the actual shape. The endpoint's `extract_claim_string`
implementation depends on this.

**Empty result handling:** If the filter matches 0 receipts, return
`{"count": 0, "receipts": []}` — not an error. The caller (chain_render) handles empty
gracefully per §4.

---

## 3. chain_render Tool Signature Change

### Source parameter

Add `source` to `parameters_schema()`:

```rust
fn parameters_schema(&self) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "enum": ["local", "foundation"],
                "default": "local",
                "description": "Chain data source. \"local\" queries the operator's own \
                                machine via the ZP gate (no cookie required). \
                                \"foundation\" queries the foundation's chain \
                                (requires a zp_session cookie — use from \
                                app.zeropointfoundation.org)."
            },
            "claim_pattern": {
                "type": "string",
                "description": "Optional claim filter, glob style (e.g. \"onboard:*\"). \
                                Defaults to onboard:* — the onboarding workflow.",
                "default": "onboard:*"
            }
        }
    })
}
```

`"local"` is the default; absent `source` is treated as `"local"`.

### Updated description

```rust
fn description(&self) -> &str {
    "Render the operator's audit-receipt chain as a Sage-voiced narration. \
     Use when the operator asks about their onboarding history, what happened \
     in their ceremony, or to see their chain. \
     \
     source=local (default): queries the operator's own machine via the ZP gate \
     at localhost:17010 — no foundation session required. \
     source=foundation: queries the foundation's chain — requires a zp_session cookie, \
     use from app.zeropointfoundation.org."
}
```

### Struct changes

Add `zp_client` and `local_base_url` fields to `ChainRenderTool`:

```rust
pub struct ChainRenderTool {
    llm: Arc<dyn ironclaw_llm::LlmProvider>,
    http: Client,
    base_url: String,          // foundation base URL (existing field)
    zp_client: Option<Arc<crate::zp::ZpClient>>,  // NEW: local gate client
}
```

`local_base_url` is derived from the `ZpConfig` (default `http://localhost:17010`). It is NOT
`base_url` — `base_url` remains the foundation URL. Both fields coexist; `source` selects which
is used.

### Registration change

`src/tools/registry.rs:610` — `register_chain_render_tool` takes an additional
`Option<Arc<ZpClient>>` parameter (or takes it from the registry's shared state).

`src/app.rs:554` — pass the `Arc<ZpClient>` (stored separately after the refactor below) to
`register_chain_render_tool`.

### Execute dispatch

```rust
async fn execute(
    &self,
    params: serde_json::Value,
    ctx: &JobContext,
) -> Result<ToolOutput, ToolError> {
    let start = std::time::Instant::now();
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("local");

    let (receipts, anchor_yaml) = match source {
        "foundation" => self.fetch_foundation(params, ctx).await?,
        _ => self.fetch_local(params).await?,
    };

    // ... existing composition logic unchanged ...
}
```

`fetch_foundation` contains the current `execute()` body (session check + HTTP calls to
foundation URL).

`fetch_local` uses `self.zp_client` to call the local gate's new endpoint via envelope auth:

```rust
async fn fetch_local(
    &self,
    params: serde_json::Value,
) -> Result<(Vec<serde_json::Value>, String), ToolError> {
    let zp = self.zp_client.as_ref().ok_or_else(|| {
        ToolError::ExecutionFailed(
            "chain_render local source unavailable: ZP gate client not configured \
             (IRONCLAW_ZP_ENABLED is unset or false). Set IRONCLAW_ZP_ENABLED=true \
             and ensure genesis.json is present."
                .to_string(),
        )
    })?;

    let claim_pattern = params
        .get("claim_pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("onboard:*");

    let path = format!(
        "/api/v1/operator/chain?claim_pattern={}",
        urlencoding::encode(claim_pattern)
    );
    let resp = zp.chain_query(&path).await.map_err(|e| {
        ToolError::ExecutionFailed(format!(
            "chain_render local source failed: ZP gate unreachable — {e}. \
             Start the gate with `zp serve` and retry."
        ))
    })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        ToolError::ExecutionFailed(format!("Local chain response was not valid JSON: {e}"))
    })?;

    let receipts = body
        .get("receipts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            ToolError::ExecutionFailed("receipts field missing from local chain response".to_string())
        })?
        .clone();

    // Voice anchor: served by the local gate at the same path as foundation.
    // If not available locally, fall back to the hardcoded embedded YAML.
    let anchor_yaml = self.fetch_anchor_yaml_or_default().await;

    Ok((receipts, anchor_yaml))
}
```

### New ZpClient method

Add to `src/zp/client.rs`:

```rust
const PATH_OPERATOR_CHAIN: &str = "/api/v1/operator/chain";

/// `GET /api/v1/operator/chain`. Fetches the local chain's receipts,
/// optionally filtered by `path_and_query` (which must start with the
/// constant path above and may include `?claim_pattern=…`).
pub async fn chain_query(
    &self,
    path_and_query: &str,
) -> Result<reqwest::Response, ZpError> {
    let resp = self
        .signed_request(Method::GET, path_and_query, vec![])
        .await?;
    Self::map_status(resp).await
}
```

---

## 4. Error Message Updates

### local source — ZP gate unreachable

**Current:** N/A (local source didn't exist).

**New:**

```
chain_render local source failed: ZP gate unreachable — <transport error>.
Start the gate with `zp serve` and retry.
```

### local source — ZP client not configured

```
chain_render local source unavailable: ZP gate client not configured
(IRONCLAW_ZP_ENABLED is unset or false). Set IRONCLAW_ZP_ENABLED=true
and ensure genesis.json is present.
```

### local source — 0 receipts

Not an error. Return explicit empty-state narration:

```
No chain receipts found matching 'onboard:*'. Your onboarding may still be in progress,
or the local gate at localhost:17010 has not yet recorded any matching receipts.
```

Implement in the existing `receipts.is_empty()` branch by inspecting source:

```rust
if receipts.is_empty() {
    let msg = match source {
        "foundation" => format!(
            "No receipts found for operator '{}' matching '{}'. \
             Either the onboarding ceremony has not run, or this session \
             is bound to a different operator.",
            session.operator_id, claim_pattern
        ),
        _ => format!(
            "No chain receipts found matching '{}'. Your onboarding may still be \
             in progress, or the local gate at localhost:17010 has not yet recorded \
             any matching receipts.",
            claim_pattern
        ),
    };
    return Err(ToolError::ExecutionFailed(msg));
}
```

### foundation source — cookie missing (updated)

**Current:**

```
Foundation session not present on this request — chain rendering requires a zp_session cookie.
Sign in at zeropointfoundation.org and retry from app.zeropointfoundation.org.
```

**New (scoped to foundation source):**

```
chain_render foundation source failed: zp_session cookie not present on this request.
Sign in at zeropointfoundation.org and access from app.zeropointfoundation.org.
Or use source=local to query your local chain — no cookie required.
```

The old error message fires only when `source == "foundation"`. The default path (`source == "local"`)
never mentions the cookie.

---

## 5. Tests

All new tests go in `src/tools/builtin/chain_render.rs` under `mod tests`.

### T1 — local source succeeds without cookie

```rust
#[tokio::test]
async fn local_source_succeeds_without_substrate_session() {
    // JobContext has no substrate_session — mirrors a local operator request.
    let ctx = JobContext::new("test_user", "test job");
    assert!(ctx.substrate_session.is_none());

    // Mock local gate via axum, same pattern as existing mock_foundation helper.
    let receipts = serde_json::json!([
        {"claim": "onboard:identity:generated"},
        {"claim": "onboard:complete", "metadata": {"voice_selection": "bm_daniel"}}
    ]);
    let base = start_mock_local_gate(receipts, "voice: sage").await;

    let stub_llm = Arc::new(StubLlm::new("You generated your keypair."));
    let tool = ChainRenderTool::with_zp_client(
        stub_llm,
        Arc::new(fake_zp_client_for_base(&base)),
    );

    let result = tool
        .execute(serde_json::json!({"source": "local"}), &ctx)
        .await
        .expect("local source must succeed without substrate_session");

    let text = result.result.as_str().unwrap();
    assert!(text.contains("2 receipts, oldest first."), "opener: {text}");
    assert!(text.contains("Chain sealed. Voice: bm_daniel."), "closer: {text}");
}
```

### T2 — foundation source still requires cookie

```rust
#[tokio::test]
async fn foundation_source_requires_substrate_session() {
    let tool = ChainRenderTool::new(Arc::new(StubLlm::new("narration")));
    let ctx = JobContext::new("test", "no session"); // no substrate_session

    let result = tool
        .execute(serde_json::json!({"source": "foundation"}), &ctx)
        .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("foundation source failed"),
                "error must name the source: {msg}"
            );
            assert!(
                msg.contains("source=local"),
                "error must suggest local alternative: {msg}"
            );
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}
```

### T3 — default source is local

```rust
#[tokio::test]
async fn absent_source_defaults_to_local() {
    let ctx = JobContext::new("test", "no session");
    let base = start_mock_local_gate(
        serde_json::json!([{"claim": "onboard:complete"}]),
        "voice: sage",
    )
    .await;

    let tool = ChainRenderTool::with_zp_client(
        Arc::new(StubLlm::failing("should not be called")),
        Arc::new(fake_zp_client_for_base(&base)),
    );

    // No source param — must not attempt foundation, must not check substrate_session.
    let result = tool.execute(serde_json::json!({}), &ctx).await;
    assert!(result.is_ok(), "default source must succeed without cookie: {result:?}");
}
```

### T4 — local source error message names gate and suggests fix

```rust
#[tokio::test]
async fn local_source_error_names_gate_when_unreachable() {
    let ctx = JobContext::new("test", "no session");
    // Point ZpClient at an unreachable port.
    let tool = ChainRenderTool::with_zp_client(
        Arc::new(StubLlm::failing("unused")),
        Arc::new(fake_zp_client_for_base("http://127.0.0.1:1")), // nothing listening
    );

    match tool.execute(serde_json::json!({"source": "local"}), &ctx).await {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(msg.contains("zp serve"), "must suggest `zp serve`: {msg}");
            assert!(!msg.contains("zp_session"), "must not mention cookie: {msg}");
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}
```

### T5 — foundation source error message names alternative

Covered by T2 assertion `msg.contains("source=local")`.

### T6 — existing tests must still pass

The existing `errors_when_substrate_session_absent` test name becomes misleading after this
change. Rename it to `foundation_source_errors_when_substrate_session_absent` and update the
assertion to check for `"foundation source failed"` instead of `"Foundation session"`.

All other existing tests (`closer_is_rust_line_even_when_llm_drifts`,
`opener_and_closer_are_identical_across_runs`, `single_receipt_chain_skips_llm`) remain valid —
they drive `execute()` through `ctx_with_session()` which has `substrate_session` set, so they
hit the foundation source path and continue to work.

---

## 6. Implementation Files

### IronClaw repo

| File | Change |
|------|--------|
| `src/tools/builtin/chain_render.rs` | Add `source` param, `zp_client` field, `fetch_local()`, `fetch_foundation()` split, updated error messages, constructor variants, tests T1–T6 |
| `src/zp/client.rs` | Add `chain_query(path_and_query)` method and `PATH_OPERATOR_CHAIN` constant |
| `src/app.rs` | Store `Arc<ZpClient>` separately before passing to hook; wire into `ChainRenderTool` registration |
| `src/tools/registry.rs` | `register_chain_render_tool` accepts `Option<Arc<ZpClient>>` |
| `src/context/state.rs` | Add `zp_client: Option<Arc<crate::zp::ZpClient>>` field to `JobContext` *(if tool accesses it through context rather than through constructor)* |
| `src/agent/dispatcher.rs` | Pass `zp_client` when building `JobContext` *(if context approach used)* |

**Preferred wiring approach — constructor injection, not context:**
Pass `Arc<ZpClient>` at `ChainRenderTool::new()` time (stored in the struct field), not via
`JobContext`. This avoids adding a ZP-specific field to the general-purpose job context and keeps
the coupling localized to the tool's constructor. `JobContext` stays clean.

The tradeoff: `ChainRenderTool` must be built after `ZpClient` is available. Currently the tool
is registered via `tools.register_chain_render_tool(Arc::clone(llm))` at `src/app.rs:554`, which
runs in the same startup block that initializes `ZpClient` (L1177). Reorder: create `ZpClient`
first (or store `Arc<ZpClient>` before consuming it in the hook), then pass both to
`register_chain_render_tool`.

**`src/app.rs` refactor (minimal):**

```rust
// Before:
match crate::zp::ZpClient::new(&zp_cfg, signer) {
    Ok(client) => {
        let hook = Arc::new(crate::zp::ZpHook::new(Arc::new(client)));
        hooks.register(hook).await;
        ...
    }
}

// After:
match crate::zp::ZpClient::new(&zp_cfg, signer) {
    Ok(client) => {
        let zp_client = Arc::new(client);
        let hook = Arc::new(crate::zp::ZpHook::new(Arc::clone(&zp_client)));
        hooks.register(hook).await;
        zp_client_for_tools = Some(zp_client); // store for later
        ...
    }
}

// ... then at tool registration (L554):
tools.register_chain_render_tool(Arc::clone(&llm), zp_client_for_tools.clone());
```

### ZP repo

| File | Change |
|------|--------|
| `crates/zp-server/src/lib.rs` | Route registration: `.route("/api/v1/operator/chain", get(operator_chain_handler))` guarded by `EnvelopeMiddleware` |
| `crates/zp-server/src/chain.rs` (new) or `handlers.rs` | `operator_chain_handler`, `ChainQueryParams`, `ChainQueryResponse`, `ChainReceiptView`, `extract_claim_string`, `glob_matches` |

**Glob matching for claim patterns:**
SQLite `LIKE` syntax is available via `json_extract`, but glob semantics differ from the
foundation's `claim_pattern` behavior. Implement a simple in-Rust prefix/glob matcher:
`onboard:*` → `claim.starts_with("onboard:")`, `*` → always matches. Full glob (`?`, `[abc]`)
is not required for initial implementation.

---

## 7. Migration / Backwards Compatibility

### Existing callers of chain_render

Sage's tool invocations currently say "show me my chain" or "what happened in my ceremony" —
these produce `chain_render` calls with no `source` parameter. After this change, absent `source`
defaults to `"local"`. For operators running locally (the common case), this is a transparent
improvement: the call succeeds instead of failing.

**Foundation-surface operators** (authenticated via `zp_session` on `app.zeropointfoundation.org`):
Their requests have `substrate_session` set AND `IRONCLAW_ZP_ENABLED` may be true. With
`source=local` as the default, a foundation-surface request will now attempt the local gate
first, not the foundation chain.

For foundation-surface operators, the local chain and the foundation chain are different datasets:
the local chain has IronClaw's tool-call governance receipts; the foundation chain has the
onboarding ceremony receipts. If a foundation-surface operator asks "show me my onboarding chain",
the default `source=local` will query IronClaw's local audit trail — which may return 0 results
or unrelated entries.

**Mitigation options** (pick one before shipping):

A. **Default stays `"local"`; foundation-surface path upgrades to pass `source=foundation`.**
   In the gateway middleware that sets `substrate_session`, also set a request attribute
   `preferred_chain_source = "foundation"`. The tool reads this and uses it as the default
   when `source` is absent. This keeps the parameter API clean while letting the context
   inform the default.

B. **Default is `"local"`; Sage's system prompt mentions the parameter.** Foundation-surface
   operators who want their ceremony chain learn to say "show me my foundation chain" or Sage
   infers it. Lower engineering cost; slightly worse UX for the dual-surface case.

C. **Default infers from session presence.** If `substrate_session` is set and `source` is
   absent, default to `"foundation"`. If no session and no ZP client, fail descriptively.
   This preserves today's foundation-surface behavior without code changes to callers.

**Recommended: Option C** — it degrades gracefully and requires no additional context or prompt
changes. Implement as:

```rust
let source = params
    .get("source")
    .and_then(|v| v.as_str())
    .unwrap_or_else(|| {
        // Infer: if substrate_session is present, caller is on the foundation surface.
        if ctx.substrate_session.is_some() { "foundation" } else { "local" }
    });
```

### Routing the `errors_when_substrate_session_absent` test

The test by that name exercises the current behavior where ANY request without a session errors.
After the change, the equivalent behavior is confined to `source=foundation`. Rename the test
to `foundation_source_errors_when_substrate_session_absent` and add `source: "foundation"` to
the params JSON. The original assertion continues to hold for that source; the renamed test is
not a behavioral regression, only a scope clarification.

### No Sage prompt changes required

The `chain_render` tool descriptor update (description + `source` enum in `parameters_schema`)
is the load-bearing change for model awareness. Sage reads the schema and will use `source=local`
or `source=foundation` when the operator's intent is explicit. Implicit requests get the inferred
default per Option C above. No prompt overlay changes are needed at initial ship.

---

## Acceptance Criteria (Reference)

1. `chain_render` at `http://127.0.0.1:3000` with no source param returns local chain successfully — no cookie required.
2. Error messages are operator-actionable: local source errors name the gate and `zp serve`; foundation source errors name the cookie and suggest `source=local`.
3. Foundation source still works on `app.zeropointfoundation.org` with a valid cookie.
4. Tool descriptor exposes `source` parameter so Sage can select it explicitly.
5. Local source uses `ZpClient.chain_query()` — the envelope-signed call to `localhost:17010`.
6. Tests T1–T6 pass under `cargo test`.

---

## Open Questions for Implementer

1. **Claim string location in local `AuditEntry`:** Run the SQL query in §2 against a live
   `audit.db` that has onboarding receipts. The `extract_claim_string` implementation in
   `zp-server` depends on which JSON field carries the `onboard:*` string.

2. **Voice anchor for local source:** The foundation serves
   `/narratives/foundation-director-onboarding.yaml` as a public static asset. The local gate
   at `localhost:17010` has no equivalent. Options: (a) embed the YAML in IronClaw as a
   compile-time `include_str!()` fallback, (b) have the local gate serve the same file from a
   bundled asset. Option (a) is lower complexity.

3. **`ChainRenderTool::with_zp_client` for tests:** Add a test constructor alongside the existing
   `with_base_url`. The test helper `fake_zp_client_for_base` needs to construct a `ZpClient`
   with a stub signer — check if `zp_keys::testing` has a stub key facility, or derive a
   throwaway key with `SigningKey::from_bytes(&[42u8; 32])`.

4. **ZP repo PR ordering:** The IronClaw `fetch_local()` call will fail until the ZP gate
   endpoint exists. Either: (a) ship ZP change first, (b) ship both simultaneously, or (c)
   ship IronClaw with a feature flag on the local source. Option (b) is cleanest for a
   coordinated substrate+agent release.
