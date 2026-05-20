//! HTTP client for the ZP cognition-governance API.
//!
//! Authentication is per-request Genesis-signed envelopes (the `ZP-Sig`
//! Authorization scheme). The client holds a Genesis-derived Ed25519
//! signer; every gate call constructs an `EnvelopeClaims` over
//! `(method, path, body_hash, ts, nonce)`, signs it, and ships the result
//! as `Authorization: ZP-Sig v=1, kid=…, ts=…, nonce=…, sig=…`. The
//! substrate's gate re-derives the same kid from the same Genesis and
//! verifies the signature. See zeropoint design doc
//! `docs/handoffs/genesis-signed-gate-requests-design-2026-05.md`.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer as DalekSigner, SigningKey};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::json;

use zp_gate_envelope::{
    EnvelopeClaims, SCHEME_VERSION, body_hash_hex, build_header, random_nonce_b64,
};
use zp_receipt::Signable;

use crate::zp::config::ZpConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const PATH_GATE_TOOL_CALL: &str = "/api/v1/gate/tool-call";
const PATH_COGNITION_OBSERVE: &str = "/api/v1/cognition/observe";
const PATH_AUDIT_RECEIPTS: &str = "/api/v1/audit/receipts";

/// Errors emitted by [`ZpClient`].
#[derive(Debug, thiserror::Error)]
pub enum ZpError {
    #[error("zp transport: {0}")]
    Transport(String),

    /// 401/403 from ZP. The hook distinguishes transient envelope failures
    /// (drift, replay) from structural ones (signer, version, malformed) so
    /// only structural failures latch the session-wide disable.
    #[error("zp authentication failed: status {status} (reason: {reason})")]
    Auth { status: u16, reason: String },

    #[error("zp server error: status {status}: {body}")]
    Server { status: u16, body: String },
}

impl ZpError {
    /// True when the auth failure is structural — wrong signer, unknown
    /// scheme version, or malformed envelope. The hook latches the
    /// session-wide disable on these. False for transient failures (drift,
    /// replay) where the next request can succeed.
    pub fn is_structural_auth(&self) -> bool {
        match self {
            ZpError::Auth { reason, .. } => matches!(
                reason.as_str(),
                "envelope-signer"
                    | "envelope-version"
                    | "envelope-malformed"
                    | "envelope-not-configured"
            ),
            _ => false,
        }
    }
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
/// Holds a `reqwest::Client` and a Genesis-derived Ed25519 signer; every
/// request goes through [`Self::signed_request`] which canonicalizes the
/// preimage and builds the `Authorization: ZP-Sig …` header. `Clone` is
/// intentionally not derived — callers wrap in `Arc<ZpClient>` from the hook.
pub struct ZpClient {
    http: Client,
    base_url: String,
    agent: String,
    signer: Arc<SigningKey>,
    kid: [u8; 32],
}

impl ZpClient {
    /// Build a new client. Returns a transport error if the underlying
    /// `reqwest::Client` cannot be constructed (system-level — TLS roots,
    /// etc.).
    pub fn new(cfg: &ZpConfig, signer: Arc<SigningKey>) -> Result<Self, ZpError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let kid = signer.verifying_key().to_bytes();
        Ok(Self {
            http,
            base_url: cfg.base_url.clone(),
            agent: cfg.agent_name.clone(),
            signer,
            kid,
        })
    }

    /// Lowercase-hex signer pubkey. Useful for diagnostics / log lines so
    /// operators can confirm the signer matches the gate's expected_kid.
    pub fn kid_hex(&self) -> String {
        hex::encode(self.kid)
    }

    /// Build a signed envelope and POST it. Single insertion point for the
    /// `Authorization: ZP-Sig …` scheme: both endpoints route through here
    /// so the envelope's `body_hash` always matches the wire bytes.
    async fn signed_request(
        &self,
        method: Method,
        path_and_query: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ZpError> {
        let claims = EnvelopeClaims {
            v: SCHEME_VERSION,
            method: method.as_str().to_string(),
            path: path_and_query.to_string(),
            body_hash: body_hash_hex(&body),
            ts: chrono::Utc::now().timestamp(),
            nonce: random_nonce_b64(),
        };
        let sig = self.signer.sign(&claims.canonical_hash()).to_bytes();
        let header = build_header(&claims, &self.kid, &sig);

        let url = format!("{}{}", self.base_url, path_and_query);
        let resp = self
            .http
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, header)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        Ok(resp)
    }

    /// Map a non-success response into the appropriate `ZpError`. Reads the
    /// `X-Auth-Reason` header on 401/403 so callers can distinguish
    /// transient failures (drift, replay) from structural ones.
    async fn map_status(resp: reqwest::Response) -> Result<reqwest::Response, ZpError> {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let reason = resp
                .headers()
                .get("x-auth-reason")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            return Err(ZpError::Auth {
                status: status.as_u16(),
                reason,
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ZpError::Server {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp)
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
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ZpError::Transport(format!("serialize gate body: {}", e)))?;

        let resp = self
            .signed_request(Method::POST, PATH_GATE_TOOL_CALL, body_bytes)
            .await?;
        let resp = Self::map_status(resp).await?;
        let decision = resp.json::<GateDecision>().await?;
        Ok(decision)
    }

    /// The base URL this client targets (e.g. `http://localhost:17010`).
    /// Exposed so callers can surface it in error messages.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/v1/audit/receipts` — fetch normalized chain entries from the
    /// local ZP gate. Returns the `receipts` array as raw JSON values using
    /// the same schema as the foundation endpoint (`{id, claim, metadata,
    /// created_at}`), so chain_render renders both sources identically.
    pub async fn fetch_chain(
        &self,
        claim_pattern: &str,
    ) -> Result<Vec<serde_json::Value>, ZpError> {
        let path = format!(
            "{}?claim_pattern={}",
            PATH_AUDIT_RECEIPTS,
            urlencoding::encode(claim_pattern)
        );
        let resp = self.signed_request(Method::GET, &path, vec![]).await?;
        let resp = Self::map_status(resp).await?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ZpError::Transport(format!("chain response not valid JSON: {e}")))?;
        let receipts = body
            .get("receipts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(receipts)
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
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ZpError::Transport(format!("serialize observe body: {}", e)))?;

        let resp = self
            .signed_request(Method::POST, PATH_COGNITION_OBSERVE, body_bytes)
            .await?;
        Self::map_status(resp).await?;
        Ok(())
    }
}
