//! HTTP client for the ZP cognition-governance API.

use std::time::Duration;

use reqwest::Client;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::json;

use crate::zp::config::ZpConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors emitted by [`ZpClient`].
#[derive(Debug, thiserror::Error)]
pub enum ZpError {
    #[error("zp transport: {0}")]
    Transport(String),

    /// 401/403 from ZP. The hook treats this as "disable for the rest of the
    /// session" (log once, don't keep hammering the gate).
    #[error("zp authentication failed: status {status}")]
    Auth { status: u16 },

    #[error("zp server error: status {status}: {body}")]
    Server { status: u16, body: String },
}

impl From<reqwest::Error> for ZpError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

/// Decoded gate response from `POST /api/v1/gate/tool-call`.
#[derive(Debug, Clone, Deserialize)]
pub struct GateDecision {
    /// `false` means the tool call must be denied.
    pub allowed: bool,

    /// Human-readable reason populated when `allowed=false`. Surfaced to
    /// the caller as a `HookOutcome::Reject` reason.
    #[serde(default)]
    pub reason: Option<String>,

    /// Receipt ID of this gate decision on the audit chain. Used as the
    /// observation's `chain_parent_receipt_id` after the turn completes.
    #[serde(default)]
    pub receipt_id: Option<String>,
}

/// Minimal HTTP client for the two ZP endpoints IronClaw posts to.
///
/// Holds a `reqwest::Client` and the bearer token; `Clone` is intentionally
/// not derived — callers wrap in `Arc<ZpClient>` from the hook.
pub struct ZpClient {
    http: Client,
    base_url: String,
    bearer: String,
    agent: String,
}

impl ZpClient {
    /// Build a new client. Returns a transport error if the underlying
    /// `reqwest::Client` cannot be constructed (system-level — TLS roots,
    /// etc.).
    pub fn new(cfg: &ZpConfig) -> Result<Self, ZpError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            http,
            base_url: cfg.base_url.clone(),
            bearer: format!("Bearer {}", cfg.session_token.expose_secret()),
            agent: cfg.agent_name.clone(),
        })
    }

    /// `POST /api/v1/gate/tool-call`. The args are NOT sent — only their
    /// blake3 hex hash, so secrets in tool params never leave the host.
    pub async fn gate_tool_call(
        &self,
        tool_name: &str,
        args_hash: &str,
        thread_id: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<GateDecision, ZpError> {
        let body = json!({
            "tool_name": tool_name,
            "args_hash": args_hash,
            "thread_id": thread_id,
            "run_id": run_id,
            "agent": &self.agent,
        });

        let resp = self
            .http
            .post(format!("{}/api/v1/gate/tool-call", self.base_url))
            .header("authorization", &self.bearer)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZpError::Auth {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ZpError::Server {
                status: status.as_u16(),
                body,
            });
        }

        let decision = resp.json::<GateDecision>().await?;
        Ok(decision)
    }

    /// `POST /api/v1/cognition/observe`. Tier-1 heuristic path: sends the
    /// raw `[user, assistant]` message pair so ZP's pipeline can run
    /// `observe_tier1()` without an extra LLM call.
    pub async fn observe(
        &self,
        user_input: &str,
        assistant_response: &str,
        chain_parent_receipt_id: Option<&str>,
    ) -> Result<(), ZpError> {
        let body = json!({
            "messages": [
                ["user", user_input],
                ["assistant", assistant_response],
            ],
            "chain_parent_receipt_id": chain_parent_receipt_id,
        });

        let resp = self
            .http
            .post(format!("{}/api/v1/cognition/observe", self.base_url))
            .header("authorization", &self.bearer)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ZpError::Auth {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ZpError::Server {
                status: status.as_u16(),
                body,
            });
        }

        Ok(())
    }
}
