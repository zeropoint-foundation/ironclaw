//! Chain-render tool — agent-rendered substrate UX PoC.
//!
//! Fetches the calling operator's audit-receipt chain from the foundation
//! workspace, loads a Sage-voice anchor, and narrates the chain using a
//! deterministic opener/closer with an LLM-voiced middle. The opener and
//! closer are produced by Rust functions reading receipt data directly —
//! they cannot drift regardless of what the LLM produces. The LLM voices
//! only the middle receipts (all but the last), and its output is truncated
//! to exactly N-1 lines before concatenation, making closer drift impossible
//! by construction (see docs/handoffs/composition-rules-slice-design-2026-05.md).
//!
//! Authentication: `source=local` (default) uses a Genesis-signed envelope
//! via `ZpClient` to call the local ZP gate — no cookie required. `source=foundation`
//! relies on a `zp_session` token in `JobContext.substrate_session` and calls
//! `zeropointfoundation.org`. Absent either, the tool returns a descriptive,
//! operator-actionable error (per `.claude/rules/tool-evidence.md`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::context::{JobContext, SubstrateSessionInfo};
use crate::tools::tool::{Tool, ToolError, ToolOutput};
use crate::zp::client::ZpClient;

/// Default base URL for the foundation workspace. Overridable via
/// `FOUNDATION_BASE_URL` for staging variants — see `wrangler.toml`'s
/// `[env.staging]` block.
const DEFAULT_FOUNDATION_BASE_URL: &str = "https://zeropointfoundation.org";

const CHAIN_PATH: &str = "/api/operator/me/chain";
const NARRATIVE_PATH: &str = "/narratives/foundation-director-onboarding.yaml";

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LLM_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_NARRATION_TOKENS: u32 = 2000;

pub struct ChainRenderTool {
    llm: Arc<dyn ironclaw_llm::LlmProvider>,
    http: Client,
    /// Base URL for the foundation voice anchor and foundation chain endpoint.
    base_url: String,
    /// Optional ZP gate client for `source=local`. `None` when ZP is not
    /// configured (standalone IronClaw without a running ZP gate).
    zp_client: Option<Arc<ZpClient>>,
}

impl ChainRenderTool {
    pub fn new(llm: Arc<dyn ironclaw_llm::LlmProvider>, zp_client: Option<Arc<ZpClient>>) -> Self {
        let base_url = std::env::var("FOUNDATION_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_FOUNDATION_BASE_URL.to_string());
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            llm,
            http,
            base_url,
            zp_client,
        }
    }

    /// Test constructor: foundation-only (no ZP gate client), custom base URL.
    #[cfg(test)]
    pub fn with_base_url(llm: Arc<dyn ironclaw_llm::LlmProvider>, base_url: String) -> Self {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            llm,
            http,
            base_url,
            zp_client: None,
        }
    }

    /// Test constructor: local ZP gate + custom base URL for voice anchor.
    #[cfg(test)]
    pub fn with_zp_client(
        llm: Arc<dyn ironclaw_llm::LlmProvider>,
        zp_client: Arc<ZpClient>,
        base_url: String,
    ) -> Self {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            llm,
            http,
            base_url,
            zp_client: Some(zp_client),
        }
    }

    /// Fetch receipts from the foundation endpoint using a `zp_session` token.
    async fn fetch_foundation_receipts(
        &self,
        session: &SubstrateSessionInfo,
        claim_pattern: &str,
    ) -> Result<Vec<serde_json::Value>, ToolError> {
        let chain_url = format!(
            "{}{}?claim_pattern={}",
            self.base_url,
            CHAIN_PATH,
            urlencoding::encode(claim_pattern)
        );
        let chain_resp = self
            .http
            .get(&chain_url)
            .header("Authorization", format!("Bearer {}", session.session_token))
            .send()
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("Foundation chain fetch failed: {e}"))
            })?;

        if !chain_resp.status().is_success() {
            let status = chain_resp.status();
            let body = chain_resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(ToolError::ExecutionFailed(format!(
                "Foundation chain endpoint returned {status}: {body}"
            )));
        }

        let chain_body: serde_json::Value = chain_resp.json().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Chain response was not valid JSON: {e}"))
        })?;

        chain_body
            .get("receipts")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .ok_or_else(|| {
                ToolError::ExecutionFailed("receipts field missing or not array".to_string())
            })
    }
}

// ── Composition helpers ──────────────────────────────────────────────────────

/// Produces the deterministic opener line from the receipt count.
/// Never calls the LLM; cannot drift.
fn format_opener(receipts: &[serde_json::Value]) -> String {
    let n = receipts.len();
    if n == 1 {
        "1 receipt, oldest first.".to_string()
    } else {
        format!("{n} receipts, oldest first.")
    }
}

/// Strips the first colon-delimited namespace segment from a claim string,
/// replaces remaining colons with spaces, and capitalizes the first character.
/// `"onboard:identity:generated"` → `"Identity generated"`.
fn humanize_claim(claim: &str) -> String {
    let stripped = match claim.find(':') {
        Some(pos) => &claim[pos + 1..],
        None => claim,
    };
    let spaced = stripped.replace(':', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Produces the deterministic closer line from the last receipt's claim and
/// optional metadata. Never calls the LLM; cannot drift.
///
/// Rules (in matching order):
/// 1. `onboard:complete` with `metadata.voice_selection` → "Chain sealed. Voice: {voice}."
/// 2. `onboard:complete` (no voice) → "Chain sealed."
/// 3. Fallback → "{humanized claim}."
fn format_closer(last_receipt: &serde_json::Value) -> String {
    let claim = last_receipt
        .get("claim")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if claim == "onboard:complete" {
        let voice = last_receipt
            .get("metadata")
            .and_then(|m| m.get("voice_selection"))
            .and_then(|v| v.as_str());
        return match voice {
            Some(v) => format!("Chain sealed. Voice: {v}."),
            None => "Chain sealed.".to_string(),
        };
    }

    format!("{}.", humanize_claim(claim))
}

/// Takes the first `n` lines from `s`. Used to truncate LLM middle output to
/// exactly the number of middle receipts, structurally preventing closer drift
/// even if the model produces extra lines.
fn take_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

// ── Tool implementation ──────────────────────────────────────────────────────

#[async_trait]
impl Tool for ChainRenderTool {
    fn name(&self) -> &str {
        "chain_render"
    }

    fn description(&self) -> &str {
        "Render the calling operator's audit-receipt chain as a Sage-voiced \
         narration. Use this when the operator asks about their chain, recent \
         substrate activity, what tools ran, what gate decisions were made, or \
         what happened in their onboarding ceremony. \
         source=local (default) queries the local substrate — no cookie required. \
         source=foundation queries the onboarding ceremony chain at \
         zeropointfoundation.org — requires a zp_session cookie."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "enum": ["local", "foundation"],
                    "default": "local",
                    "description": "Chain source. 'local' (default) queries the local \
                                    substrate via Genesis-signed envelope auth — shows \
                                    tool lifecycle, gate decisions, and delegation events. \
                                    No cookie required. 'foundation' queries \
                                    zeropointfoundation.org via zp_session cookie — shows \
                                    the onboarding ceremony chain."
                },
                "claim_pattern": {
                    "type": "string",
                    "description": "Optional glob filter (trailing * only). Defaults to \
                                    '*' for local source (all entries) and 'onboard:*' \
                                    for foundation source."
                }
            }
        })
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

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

        // 1. Fetch receipts — path depends on source.
        let receipts: Vec<serde_json::Value> = match source {
            "foundation" => {
                let session = ctx.substrate_session.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "chain_render foundation source failed: zp_session cookie not \
                         present on this request. Sign in at zeropointfoundation.org \
                         and access from app.zeropointfoundation.org. Or omit source \
                         (or use source=local) to query your local substrate chain \
                         instead."
                            .to_string(),
                    )
                })?;
                let claim_pattern = params
                    .get("claim_pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("onboard:*");
                self.fetch_foundation_receipts(session, claim_pattern)
                    .await?
            }
            _ => {
                // "local" or any unrecognised value → local (safe default)
                let client = self.zp_client.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "chain_render local source failed: ZP gate client not \
                         configured in this IronClaw instance. Check that \
                         ZP_BASE_URL is set and `zp serve` is running."
                            .to_string(),
                    )
                })?;
                let claim_pattern = params
                    .get("claim_pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("*");
                client.fetch_chain(claim_pattern).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!(
                        "chain_render local source failed: ZP gate at {} \
                         unreachable: {}. Start the gate with `zp serve` and retry.",
                        client.base_url(),
                        e
                    ))
                })?
            }
        };

        // 2. Handle empty chain.
        if receipts.is_empty() {
            if source != "foundation" {
                return Ok(ToolOutput::text(
                    "No local chain entries yet. The local chain records tool \
                     lifecycle events, gate decisions, and delegation grants — \
                     these accumulate after `zp serve` is running and tools are \
                     launched. For the onboarding ceremony chain, use \
                     source=foundation."
                        .to_string(),
                    start.elapsed(),
                ));
            }
            return Err(ToolError::ExecutionFailed(format!(
                "No receipts found for operator '{}' matching '{}'. Either the \
                 onboarding ceremony has not run, or this session is bound to a \
                 different operator.",
                ctx.substrate_session
                    .as_ref()
                    .map(|s| s.operator_id.as_str())
                    .unwrap_or("unknown"),
                params
                    .get("claim_pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("onboard:*")
            )));
        }

        // 3. Fetch the voice anchor. Served as a public static asset — no auth.
        let anchor_url = format!("{}{}", self.base_url, NARRATIVE_PATH);
        let anchor_resp =
            self.http.get(&anchor_url).send().await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Voice anchor fetch failed: {e}"))
            })?;
        if !anchor_resp.status().is_success() {
            return Err(ToolError::ExecutionFailed(format!(
                "Voice anchor endpoint returned {} for {anchor_url}",
                anchor_resp.status()
            )));
        }
        let anchor_yaml = anchor_resp.text().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Voice anchor body read failed: {e}"))
        })?;

        // 4. Deterministic opener — count only, no LLM, cannot drift.
        let opener = format_opener(&receipts);

        // 5. Split: middle receipts (all but last) go to LLM; last goes to Rust closer.
        let last_receipt = &receipts[receipts.len() - 1];
        let closer = format_closer(last_receipt);

        // 6. LLM voices middle receipts only. Skipped entirely for single-receipt chains.
        let middle = if receipts.len() <= 1 {
            String::new()
        } else {
            let middle_receipts = &receipts[..receipts.len() - 1];
            let middle_count = middle_receipts.len();
            let plural = if middle_count == 1 { "" } else { "s" };

            let middle_json = serde_json::to_string_pretty(middle_receipts).map_err(|e| {
                ToolError::ExecutionFailed(format!("Middle receipt JSON encoding failed: {e}"))
            })?;

            let user_msg = format!(
                "VOICE ANCHOR (YAML)\n===\n{anchor_yaml}\n\n\
                 MIDDLE RECEIPTS ({middle_count} receipt{plural}, oldest first)\n===\n{middle_json}\n\n\
                 DIRECTIVE\n===\n\
                 Voice the {middle_count} middle receipt{plural} in the anchor's voice. \
                 One plain prose line per receipt, oldest first. Reference actual receipt \
                 content — fingerprints, capability names, voice selection, claim names.\n\n\
                 Do not add an opener before the first line. Do not add a closer after \
                 the last line. The substrate provides both. Start immediately with the \
                 first receipt's voicing. After voicing receipt {middle_count}, stop."
            );

            let llm_messages = vec![
                ironclaw_llm::ChatMessage::system(include_str!(
                    "../../../crates/ironclaw_engine/prompts/chain_narration.md"
                )),
                ironclaw_llm::ChatMessage::user(user_msg),
            ];
            let request = ironclaw_llm::CompletionRequest::new(llm_messages)
                .with_max_tokens(MAX_NARRATION_TOKENS);

            let response = tokio::time::timeout(LLM_TIMEOUT, self.llm.complete(request))
                .await
                .map_err(|_| {
                    ToolError::ExecutionFailed(format!(
                        "LLM narration timed out after {LLM_TIMEOUT:?}"
                    ))
                })?
                .map_err(|e| {
                    ToolError::ExecutionFailed(format!("LLM narration call failed: {e}"))
                })?;

            // Truncate to exactly middle_count lines. Even if the LLM appends a drifted
            // closer after its last middle-receipt line, take_lines discards it before
            // concatenation — the Rust closer is always the last line of the output.
            take_lines(response.content.trim(), middle_count)
        };

        // 7. Concatenate: opener + middle (if any) + closer.
        let narration = if middle.is_empty() {
            format!("{opener}\n{closer}")
        } else {
            format!("{opener}\n{middle}\n{closer}")
        };

        // 8. Sanitize against attacker-controlled receipt metadata that may have flowed
        // through the LLM and could carry a prompt-injection payload (per safety rules).
        let sanitizer = ironclaw_safety::Sanitizer::new();
        let sanitized = sanitizer.sanitize(narration.trim());
        if sanitized.was_modified {
            tracing::debug!(
                user_id = %ctx.user_id,
                "Chain narration contained suspicious patterns; content was sanitized"
            );
        }

        Ok(ToolOutput::text(sanitized.content, start.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zp::config::ZpConfig;
    use ironclaw_llm::testing::StubLlm;

    // ── Pure-function unit tests (no I/O) ────────────────────────────────────

    #[test]
    fn opener_is_count_based() {
        let receipts_16: Vec<serde_json::Value> = vec![serde_json::json!({}); 16];
        assert_eq!(format_opener(&receipts_16), "16 receipts, oldest first.");

        let receipts_1 = vec![serde_json::json!({})];
        assert_eq!(format_opener(&receipts_1), "1 receipt, oldest first.");

        let receipts_2: Vec<serde_json::Value> = vec![serde_json::json!({}); 2];
        assert_eq!(format_opener(&receipts_2), "2 receipts, oldest first.");
    }

    #[test]
    fn closer_known_claims() {
        let complete_with_voice = serde_json::json!({
            "claim": "onboard:complete",
            "metadata": {"voice_selection": "bm_daniel"}
        });
        assert_eq!(
            format_closer(&complete_with_voice),
            "Chain sealed. Voice: bm_daniel."
        );

        let complete_no_voice = serde_json::json!({"claim": "onboard:complete"});
        assert_eq!(format_closer(&complete_no_voice), "Chain sealed.");

        let complete_empty_meta = serde_json::json!({
            "claim": "onboard:complete",
            "metadata": {}
        });
        assert_eq!(format_closer(&complete_empty_meta), "Chain sealed.");

        let identity = serde_json::json!({"claim": "onboard:identity:generated"});
        assert_eq!(format_closer(&identity), "Identity generated.");

        let voice_sel = serde_json::json!({"claim": "onboard:voice:selected"});
        assert_eq!(format_closer(&voice_sel), "Voice selected.");

        let key_reg = serde_json::json!({"claim": "onboard:key:registered"});
        assert_eq!(format_closer(&key_reg), "Key registered.");
    }

    #[test]
    fn humanize_claim_strips_prefix_and_capitalizes() {
        assert_eq!(
            humanize_claim("onboard:identity:generated"),
            "Identity generated"
        );
        assert_eq!(humanize_claim("onboard:voice:selected"), "Voice selected");
        assert_eq!(humanize_claim("onboard:complete"), "Complete");
        assert_eq!(humanize_claim("nocol"), "Nocol");
        assert_eq!(humanize_claim("a:b:c:d"), "B c d");
    }

    #[test]
    fn take_lines_truncates_to_n() {
        let input = "line one\nline two\nline three\nextra drift line";
        assert_eq!(take_lines(input, 3), "line one\nline two\nline three");
        assert_eq!(take_lines(input, 1), "line one");
        assert_eq!(take_lines(input, 0), "");
        // Asking for more than available lines returns what's there.
        assert_eq!(take_lines(input, 10), input);
    }

    #[test]
    fn opener_and_closer_contain_no_list_markers() {
        let receipts = vec![
            serde_json::json!({"claim": "onboard:identity:generated"}),
            serde_json::json!({"claim": "onboard:complete", "metadata": {"voice_selection": "bm_daniel"}}),
        ];
        let opener = format_opener(&receipts);
        let closer = format_closer(receipts.last().unwrap());

        for line in [&opener, &closer] {
            assert!(!line.starts_with("- "), "no dash list in: {line}");
            assert!(!line.starts_with("* "), "no bullet in: {line}");
            assert!(
                !line.starts_with(|c: char| c.is_ascii_digit() && line.contains(". ")),
                "no numbered list in: {line}"
            );
            assert!(!line.contains("**"), "no bold in: {line}");
        }
    }

    // ── Existing session-propagation tests ───────────────────────────────────

    /// `source=foundation` without a substrate session must return a descriptive
    /// error that names the problem and suggests `source=local`.
    #[tokio::test]
    async fn foundation_source_errors_when_substrate_session_absent() {
        let tool = ChainRenderTool::new(Arc::new(StubLlm::new("narration")), None);
        let ctx = JobContext::new("test", "no session");
        let result = tool
            .execute(serde_json::json!({"source": "foundation"}), &ctx)
            .await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(
                    msg.contains("zp_session cookie not present"),
                    "error should mention missing cookie, got: {msg}"
                );
                assert!(
                    msg.contains("source=local"),
                    "error should suggest source=local, got: {msg}"
                );
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    /// `source=local` (default, no explicit param) without a ZP gate client must
    /// return a descriptive error mentioning `zp serve`.
    #[tokio::test]
    async fn local_source_errors_when_zp_client_absent() {
        let tool = ChainRenderTool::new(Arc::new(StubLlm::new("narration")), None);
        let ctx = JobContext::new("test", "no session");
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(
                    msg.contains("ZP gate client not configured"),
                    "error should mention ZP gate client, got: {msg}"
                );
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    /// Bridge regression: a JobContext built via `with_substrate_session` must
    /// carry operator_id and token through to the tool.
    #[test]
    fn job_context_propagates_substrate_session() {
        let info = SubstrateSessionInfo {
            operator_id: "ken".to_string(),
            session_token: "tok-xyz".to_string(),
        };
        let ctx =
            JobContext::new("test", "with session").with_substrate_session(Some(info.clone()));
        let stored = ctx
            .substrate_session
            .as_ref()
            .expect("substrate_session should be present");
        assert_eq!(stored.operator_id, "ken");
        assert_eq!(stored.session_token, "tok-xyz");
    }

    // ── Caller-level tests (drive execute() through a mock HTTP server) ───────

    fn ctx_with_session() -> JobContext {
        JobContext::new("test_user", "test job").with_substrate_session(Some(
            SubstrateSessionInfo {
                operator_id: "ken".to_string(),
                session_token: "tok-xyz".to_string(),
            },
        ))
    }

    /// Spin up a minimal axum server that serves canned chain + anchor responses.
    async fn start_mock_foundation(
        receipts: serde_json::Value,
        anchor: impl Into<String>,
    ) -> String {
        use axum::{Router, extract::State, routing::get};

        #[derive(Clone)]
        struct MockState {
            chain: String,
            anchor: String,
        }

        let count = receipts.as_array().map(|a| a.len()).unwrap_or(0);
        let chain_body = serde_json::json!({
            "count": count,
            "receipts": receipts
        })
        .to_string();

        let state = MockState {
            chain: chain_body,
            anchor: anchor.into(),
        };

        let app = Router::new()
            .route(
                "/api/operator/me/chain",
                get(|State(s): State<MockState>| async move {
                    ([("content-type", "application/json")], s.chain)
                }),
            )
            .route(
                "/narratives/foundation-director-onboarding.yaml",
                get(|State(s): State<MockState>| async move {
                    ([("content-type", "text/yaml")], s.anchor)
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock server bind failed");
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock server failed");
        });

        format!("http://127.0.0.1:{port}")
    }

    /// Test D (design doc): closer is always the Rust-produced line, even when
    /// the LLM appends a drift line after its last middle-receipt narration.
    /// `take_lines` truncates the drift; the Rust closer is always last.
    #[tokio::test]
    async fn closer_is_rust_line_even_when_llm_drifts() {
        // 3-receipt chain: middle has 2 receipts, LLM returns 2 middle lines + drift.
        let receipts = serde_json::json!([
            {"claim": "onboard:identity:generated"},
            {"claim": "onboard:voice:selected"},
            {"claim": "onboard:complete", "metadata": {"voice_selection": "bm_daniel"}}
        ]);
        // LLM returns 2 valid middle lines followed by a classic drift pattern.
        let stub = Arc::new(StubLlm::new(
            "You generated your keypair.\nYou selected your voice.\nLet me know if you need anything.",
        ));
        let base = start_mock_foundation(receipts, "voice: sage").await;
        let tool = ChainRenderTool::with_base_url(stub, base);

        let result = tool
            .execute(
                serde_json::json!({"source": "foundation"}),
                &ctx_with_session(),
            )
            .await
            .unwrap();
        let text = result.result.as_str().expect("result should be a string");
        let lines: Vec<&str> = text.lines().collect();

        let last_line = *lines.last().expect("output should have lines");
        assert_eq!(
            last_line, "Chain sealed. Voice: bm_daniel.",
            "Rust closer must be the last line regardless of LLM drift"
        );
        assert!(
            !text.contains("Let me know if you need anything"),
            "drift line must be truncated before concatenation"
        );
    }

    /// Test C (design doc): opener and closer are identical across two runs;
    /// both are Rust-produced, so they cannot vary between invocations.
    #[tokio::test]
    async fn opener_and_closer_are_identical_across_runs() {
        let receipts = serde_json::json!([
            {"claim": "onboard:identity:generated"},
            {"claim": "onboard:voice:selected"},
            {"claim": "onboard:complete", "metadata": {"voice_selection": "bm_daniel"}}
        ]);
        let stub = Arc::new(StubLlm::new("Middle line one.\nMiddle line two."));
        let base = start_mock_foundation(receipts, "voice: sage").await;
        let tool = ChainRenderTool::with_base_url(stub, base);

        let run1 = tool
            .execute(
                serde_json::json!({"source": "foundation"}),
                &ctx_with_session(),
            )
            .await
            .unwrap();
        let run2 = tool
            .execute(
                serde_json::json!({"source": "foundation"}),
                &ctx_with_session(),
            )
            .await
            .unwrap();

        let text1 = run1
            .result
            .as_str()
            .expect("run1 result should be a string");
        let text2 = run2
            .result
            .as_str()
            .expect("run2 result should be a string");
        let lines1: Vec<&str> = text1.lines().collect();
        let lines2: Vec<&str> = text2.lines().collect();

        assert_eq!(
            lines1.first(),
            lines2.first(),
            "opener must be identical across runs"
        );
        assert_eq!(
            lines1.last(),
            lines2.last(),
            "closer must be identical across runs"
        );

        // Verify the opener and closer are the expected Rust-produced values.
        assert_eq!(*lines1.first().unwrap(), "3 receipts, oldest first.");
        assert_eq!(*lines1.last().unwrap(), "Chain sealed. Voice: bm_daniel.");
    }

    /// Test F (design doc): a single-receipt chain skips the LLM entirely and
    /// produces a valid opener + closer output.
    #[tokio::test]
    async fn single_receipt_chain_skips_llm() {
        let receipts = serde_json::json!([
            {"claim": "onboard:complete", "metadata": {"voice_selection": "kokoro"}}
        ]);
        // If the LLM is called, StubLlm::failing() would cause an Err; test proves
        // execute() returns Ok without touching the LLM.
        let stub = Arc::new(StubLlm::failing("should-not-be-called"));
        let base = start_mock_foundation(receipts, "voice: sage").await;
        let tool = ChainRenderTool::with_base_url(stub.clone(), base);

        let result = tool
            .execute(
                serde_json::json!({"source": "foundation"}),
                &ctx_with_session(),
            )
            .await
            .expect("single-receipt chain should succeed without LLM");

        assert_eq!(stub.calls(), 0, "LLM must not be called for N=1");
        let text = result.result.as_str().expect("result should be a string");
        assert!(
            text.contains("1 receipt, oldest first."),
            "opener should appear: {text}"
        );
        assert!(
            text.contains("Chain sealed. Voice: kokoro."),
            "closer should appear: {text}"
        );
    }

    // ── Local source helpers ─────────────────────────────────────────────────

    /// Build a ZpClient pointing at the given base URL using a fixed test
    /// signing key. The mock gate doesn't verify envelope signatures.
    fn stub_zp_client(base_url: &str) -> Arc<ZpClient> {
        use ed25519_dalek::SigningKey;
        let cfg = ZpConfig {
            base_url: base_url.to_string(),
            agent_name: "test".to_string(),
            genesis_record_path: std::path::PathBuf::from("/dev/null"),
        };
        let key = Arc::new(SigningKey::from_bytes(&[1u8; 32]));
        Arc::new(ZpClient::new(&cfg, key).expect("test ZpClient"))
    }

    /// Spin up a mock server that serves both the local receipts endpoint
    /// (`/api/v1/audit/receipts`) and the voice anchor
    /// (`/narratives/foundation-director-onboarding.yaml`).
    async fn start_mock_local_gate(
        receipts: serde_json::Value,
        anchor: impl Into<String>,
    ) -> String {
        use axum::{Router, extract::State, routing::get};

        #[derive(Clone)]
        struct MockState {
            receipts: String,
            anchor: String,
        }

        let count = receipts.as_array().map(|a| a.len()).unwrap_or(0);
        let receipts_body = serde_json::json!({
            "count": count,
            "receipts": receipts
        })
        .to_string();

        let state = MockState {
            receipts: receipts_body,
            anchor: anchor.into(),
        };

        let app = Router::new()
            .route(
                "/api/v1/audit/receipts",
                get(|State(s): State<MockState>| async move {
                    ([("content-type", "application/json")], s.receipts)
                }),
            )
            .route(
                "/narratives/foundation-director-onboarding.yaml",
                get(|State(s): State<MockState>| async move {
                    ([("content-type", "text/yaml")], s.anchor)
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock local gate bind failed");
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock local gate failed");
        });

        format!("http://127.0.0.1:{port}")
    }

    // ── Local source tests ───────────────────────────────────────────────────

    /// Local source succeeds without a substrate session cookie. The tool must
    /// narrate the chain using only the ZpClient envelope path.
    #[tokio::test]
    async fn local_source_succeeds_without_substrate_session() {
        let receipts = serde_json::json!([
            {"claim": "tool:preflight:passed:ironclaw", "metadata": {}, "id": "e1", "created_at": "2026-05-19T00:00:00Z"},
            {"claim": "tool:launched:ironclaw", "metadata": {}, "id": "e2", "created_at": "2026-05-19T00:00:01Z"}
        ]);
        let stub = Arc::new(StubLlm::new("The preflight check passed."));
        let base = start_mock_local_gate(receipts, "voice: sage").await;
        let zp = stub_zp_client(&base);
        let tool = ChainRenderTool::with_zp_client(stub, zp, base);

        let ctx = JobContext::new("test_user", "no session"); // no substrate_session
        let result = tool
            .execute(serde_json::json!({}), &ctx) // default source=local
            .await
            .expect("local source should succeed without substrate session");

        let text = result.result.as_str().expect("result should be string");
        assert!(
            text.contains("2 receipts"),
            "opener should reflect receipt count: {text}"
        );
    }

    /// Empty local chain returns an Ok with a hint, not an error.
    #[tokio::test]
    async fn local_source_empty_chain_returns_hint_not_error() {
        let base = start_mock_local_gate(serde_json::json!([]), "voice: sage").await;
        let zp = stub_zp_client(&base);
        let stub = Arc::new(StubLlm::failing("should-not-be-called"));
        let tool = ChainRenderTool::with_zp_client(stub, zp, base);

        let ctx = JobContext::new("test_user", "no session");
        let result = tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("empty local chain should return Ok, not Err");

        let text = result.result.as_str().expect("result should be string");
        assert!(
            text.contains("source=foundation"),
            "hint should suggest source=foundation: {text}"
        );
    }

    /// Default source (no `source` param) is local — the tool must not attempt
    /// the foundation cookie path when no source is specified.
    #[tokio::test]
    async fn default_source_is_local_not_foundation() {
        let receipts = serde_json::json!([
            {"claim": "tool:launched:ironclaw", "metadata": {}, "id": "e1", "created_at": "2026-05-19T00:00:00Z"}
        ]);
        let stub = Arc::new(StubLlm::failing("should-not-be-called"));
        let base = start_mock_local_gate(receipts, "voice: sage").await;
        let zp = stub_zp_client(&base);
        let tool = ChainRenderTool::with_zp_client(stub, zp, base);

        // No substrate_session — would fail immediately if foundation path were taken.
        let ctx = JobContext::new("test_user", "no session");
        let result = tool.execute(serde_json::json!({}), &ctx).await;

        // Foundation path error begins with "chain_render foundation source failed"
        if let Err(ToolError::ExecutionFailed(msg)) = &result {
            assert!(
                !msg.contains("foundation source failed"),
                "default should not hit foundation path, got: {msg}"
            );
        }
        // Either Ok (narrated) or a local error — both are fine here; the test
        // only asserts the foundation cookie path was NOT triggered.
    }
}
