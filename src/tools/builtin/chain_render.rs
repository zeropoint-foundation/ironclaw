//! Chain-render tool — agent-rendered substrate UX PoC.
//!
//! Fetches the calling operator's audit-receipt chain from the foundation
//! workspace, loads a Sage-voice anchor, and asks the configured LLM to
//! narrate the chain in voice. The narration is fresh per render: this is
//! the live-agent interpretation path (`docs/AGENTIC-SURFACE-2026-05.md`),
//! not template substitution. Edge / unknown claims are handled implicitly
//! because the agent reads each receipt as data.
//!
//! Authentication: relies on a foundation `zp_session` token carried into
//! `JobContext.substrate_session` by the gateway middleware. Absent that,
//! the tool returns a descriptive error rather than a silent empty success
//! (per `.claude/rules/tool-evidence.md`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

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
    base_url: String,
}

impl ChainRenderTool {
    pub fn new(llm: Arc<dyn ironclaw_llm::LlmProvider>) -> Self {
        let base_url = std::env::var("FOUNDATION_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_FOUNDATION_BASE_URL.to_string());
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { llm, http, base_url }
    }

    #[cfg(test)]
    pub fn with_base_url(llm: Arc<dyn ironclaw_llm::LlmProvider>, base_url: String) -> Self {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { llm, http, base_url }
    }
}

#[async_trait]
impl Tool for ChainRenderTool {
    fn name(&self) -> &str {
        "chain_render"
    }

    fn description(&self) -> &str {
        "Render the calling operator's foundation audit-receipt chain as a \
         Sage-voiced narration. Use this when the operator asks about their \
         onboarding history, what happened in their ceremony, or to see \
         their chain. The narration is generated live from the receipts and \
         is grounded in the actual audit trail — there is no static script."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "claim_pattern": {
                    "type": "string",
                    "description": "Optional claim filter, glob style (e.g. \"onboard:*\"). Defaults to onboard:* — the onboarding workflow.",
                    "default": "onboard:*"
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

        let session = ctx.substrate_session.as_ref().ok_or_else(|| {
            ToolError::ExecutionFailed(
                "Foundation session not present on this request — chain rendering \
                 requires a zp_session cookie. Sign in at zeropointfoundation.org \
                 and retry from app.zeropointfoundation.org."
                    .to_string(),
            )
        })?;

        let claim_pattern = params
            .get("claim_pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("onboard:*");

        // 1. Fetch the operator's chain.
        let chain_url = format!(
            "{}{}?claim_pattern={}",
            self.base_url,
            CHAIN_PATH,
            urlencoding::encode(claim_pattern)
        );
        let chain_resp = self
            .http
            .get(&chain_url)
            .header(
                "Authorization",
                format!("Bearer {}", session.session_token),
            )
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

        let count = chain_body
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if count == 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "No receipts found for operator '{}' matching '{}'. Either the \
                 onboarding ceremony has not run, or this session is bound to a \
                 different operator.",
                session.operator_id, claim_pattern
            )));
        }

        // 2. Fetch the voice anchor. Anchor is served as a public static asset
        // (no auth) so no Authorization header.
        let anchor_url = format!("{}{}", self.base_url, NARRATIVE_PATH);
        let anchor_resp = self.http.get(&anchor_url).send().await.map_err(|e| {
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

        // 3. Build the user message. Anchor first, then receipts, then directive.
        let receipts_pretty = serde_json::to_string_pretty(
            chain_body.get("receipts").unwrap_or(&serde_json::Value::Null),
        )
        .map_err(|e| {
            ToolError::ExecutionFailed(format!("Receipt JSON re-encoding failed: {e}"))
        })?;

        let user_msg = format!(
            "VOICE ANCHOR (YAML)\n===\n{anchor_yaml}\n\n\
             RECEIPT CHAIN ({count} receipts, oldest first)\n===\n{receipts_pretty}\n\n\
             DIRECTIVE\n===\n\
             Narrate this operator's onboarding chain in voice. One line per \
             receipt, in chain order. Use the anchor's tone. Reference actual \
             receipt content. Do not summarize at the end unless the chain \
             itself ends with a completion receipt."
        );

        // 4. LLM call.
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

        // Sanitize the narration the same way memory_search does — defends
        // against attacker-controlled receipt metadata that flowed through
        // the LLM and could carry a prompt-injection payload back into the
        // chat surface (per .claude/rules/safety-and-sandbox.md).
        let sanitizer = ironclaw_safety::Sanitizer::new();
        let sanitized = sanitizer.sanitize(response.content.trim());
        if sanitized.was_modified {
            tracing::debug!(
                user_id = %ctx.user_id,
                operator_id = %session.operator_id,
                warnings = sanitized.warnings.len(),
                "Chain narration contained suspicious patterns; content was sanitized"
            );
        }

        Ok(ToolOutput::text(sanitized.content, start.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::SubstrateSessionInfo;
    use ironclaw_llm::testing::StubLlm;

    /// Without a substrate session on the JobContext, the tool must return a
    /// descriptive error rather than silently fetching an unauthenticated
    /// chain (per tool-evidence.md: empty/silent results from a side-effect
    /// tool are bugs).
    #[tokio::test]
    async fn errors_when_substrate_session_absent() {
        let tool = ChainRenderTool::new(Arc::new(StubLlm::new("narration")));
        let ctx = JobContext::new("test", "no session");
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                assert!(
                    msg.contains("Foundation session"),
                    "error should mention missing session, got: {msg}"
                );
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    /// Bridge regression test: a JobContext built via `with_substrate_session`
    /// must carry the operator_id and token through to the tool. This is the
    /// shape the dispatcher uses — see `src/agent/dispatcher.rs::chat_job_context`.
    #[test]
    fn job_context_propagates_substrate_session() {
        let info = SubstrateSessionInfo {
            operator_id: "ken".to_string(),
            session_token: "tok-xyz".to_string(),
        };
        let ctx = JobContext::new("test", "with session")
            .with_substrate_session(Some(info.clone()));
        let stored = ctx
            .substrate_session
            .as_ref()
            .expect("substrate_session should be present");
        assert_eq!(stored.operator_id, "ken");
        assert_eq!(stored.session_token, "tok-xyz");
    }
}
