//! Lifecycle hook that wires IronClaw's agent loop into ZP cognition governance.
//!
//! The hook subscribes to three points:
//!
//! - `BeforeInbound`: caches the user input keyed by `(user_id, thread_id)`.
//! - `BeforeToolCall`: sends `tool_name + blake3(args)` to ZP's gate; on
//!   `allowed=false` rejects the tool call; on `allowed=true` caches the
//!   receipt_id for later observation parenting.
//! - `TransformResponse`: posts the cached `[user, assistant]` pair to
//!   `/api/v1/cognition/observe`, with the most recent gate receipt as
//!   `chain_parent_receipt_id` (heuristic — see comment below).
//!
//! Failure mode is fail-open: any non-auth ZP error logs at debug and
//! returns `HookOutcome::Reject` with a clear "ZP unreachable" message —
//! degrade-CLOSED. Auth failures (401/403) similarly reject AND disable
//! the hook for the rest of the process via an internal flag, so a
//! single re-attempt doesn't keep hitting auth.
//!
//! See zeropoint task #91 for the load-bearing-honesty rationale: a
//! substrate that silently degrades open is worse than one that surfaces
//! the failure to the operator. Brief zp-server outages will block tool
//! dispatch until the server returns; that is the intended behavior.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::hooks::{
    Hook, HookContext, HookError, HookEvent, HookFailureMode, HookOutcome, HookPoint,
};
use crate::zp::client::{ZpClient, ZpError};

const HOOK_POINTS: &[HookPoint] = &[
    HookPoint::BeforeInbound,
    HookPoint::BeforeToolCall,
    HookPoint::TransformResponse,
];

/// `(user_id, thread_id)`. `thread_id` is `None` for stateless inbound
/// events (rare; defensive). Cache entries live until consumed by the
/// matching `TransformResponse`.
type ThreadKey = (String, Option<String>);

/// ZP cognition-governance hook.
pub struct ZpHook {
    client: Arc<ZpClient>,
    last_user_input: RwLock<HashMap<ThreadKey, String>>,
    last_gate_receipt: RwLock<HashMap<ThreadKey, String>>,
    /// Set on the first 401/403; once set, subsequent hook firings no-op.
    /// Avoids hammering the gate when our session token is rejected.
    auth_disabled: AtomicBool,
}

impl ZpHook {
    pub fn new(client: Arc<ZpClient>) -> Self {
        Self {
            client,
            last_user_input: RwLock::new(HashMap::new()),
            last_gate_receipt: RwLock::new(HashMap::new()),
            auth_disabled: AtomicBool::new(false),
        }
    }

    /// blake3 hex digest of the JSON-serialized parameters. Never logged;
    /// ZP only ever sees the hash, so tool secrets do not leave the host.
    fn args_hash(parameters: &serde_json::Value) -> String {
        // serde_json::to_vec on a Value cannot fail (the input is already
        // a parsed Value), but treat encode failures as an empty payload
        // rather than panicking.
        let bytes = serde_json::to_vec(parameters).unwrap_or_default();
        blake3::hash(&bytes).to_hex().to_string()
    }

    fn is_disabled(&self) -> bool {
        self.auth_disabled.load(Ordering::Relaxed)
    }

    fn disable(&self) {
        self.auth_disabled.store(true, Ordering::Relaxed);
    }
}

#[async_trait]
impl Hook for ZpHook {
    fn name(&self) -> &str {
        "zp.cognition_governance"
    }

    fn hook_points(&self) -> &[HookPoint] {
        HOOK_POINTS
    }

    fn failure_mode(&self) -> HookFailureMode {
        // Degrade-CLOSED. If the ZP hook itself errors (panic, etc.), the
        // governed tool call is BLOCKED. This is deliberate: a substrate
        // that "almost works" is worse than one that's honest about its
        // coverage. Better to surface the failure to the operator than to
        // silently dispatch ungoverned tool calls that look governed.
        //
        // Companion behavior in execute(): transport / 5xx errors return
        // HookOutcome::Reject with a clear "ZP unreachable" message rather
        // than silently continuing.
        //
        // Operational implication for Foundation members: brief zp-server
        // outages will block tool dispatch until the server returns. This
        // is the correct behavior — see ZP task #91 for the full rationale.
        HookFailureMode::FailClosed
    }

    async fn execute(
        &self,
        event: &HookEvent,
        _ctx: &HookContext,
    ) -> Result<HookOutcome, HookError> {
        if self.is_disabled() {
            return Ok(HookOutcome::ok());
        }

        match event {
            HookEvent::Inbound {
                user_id,
                content,
                thread_id,
                ..
            } => {
                let key = (user_id.clone(), thread_id.clone());
                self.last_user_input
                    .write()
                    .await
                    .insert(key, content.clone());
                Ok(HookOutcome::ok())
            }

            HookEvent::ToolCall {
                tool_name,
                parameters,
                user_id,
                thread_id,
                run_id,
                ..
            } => {
                let hash = Self::args_hash(parameters);
                match self
                    .client
                    .gate_tool_call(tool_name, &hash, thread_id.as_deref(), run_id.as_deref())
                    .await
                {
                    Ok(decision) => {
                        if !decision.allowed {
                            return Ok(HookOutcome::reject(
                                decision
                                    .reason
                                    .unwrap_or_else(|| "denied by zp gate".to_string()),
                            ));
                        }
                        if let Some(rid) = decision.receipt_id {
                            let key = (user_id.clone(), thread_id.clone());
                            self.last_gate_receipt.write().await.insert(key, rid);
                        }
                        // Migrate any (user_id, None) input cache to the resolved
                        // thread_id. BeforeInbound often fires before the channel
                        // resolves a thread (so it caches under None); by
                        // BeforeToolCall the thread is established. Without this
                        // promotion, TransformResponse — which always sees a
                        // resolved thread_id — would miss the cached input under
                        // (user_id, None) and silently skip the observation.
                        if thread_id.is_some() {
                            let mut cache = self.last_user_input.write().await;
                            let key_none = (user_id.clone(), None);
                            let key_resolved = (user_id.clone(), thread_id.clone());
                            if !cache.contains_key(&key_resolved)
                                && let Some(input) = cache.remove(&key_none)
                            {
                                cache.insert(key_resolved, input);
                            }
                        }
                        Ok(HookOutcome::ok())
                    }
                    Err(e @ ZpError::Auth { .. }) => {
                        let structural = e.is_structural_auth();
                        let (status, reason) = match &e {
                            ZpError::Auth { status, reason } => (*status, reason.clone()),
                            _ => unreachable!(),
                        };
                        if structural {
                            tracing::error!(
                                status,
                                reason = %reason,
                                "zp gate envelope rejected (structural); disabling hook for session"
                            );
                            self.disable();
                        } else {
                            tracing::warn!(
                                status,
                                reason = %reason,
                                "zp gate envelope rejected (transient — drift/replay); next request may succeed"
                            );
                        }
                        Ok(HookOutcome::reject(format!(
                            "ZP gate rejected envelope (status {status}, X-Auth-Reason: {reason}). \
                             Check Genesis derivation. Run `zp doctor` for diagnostics."
                        )))
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "zp gate unreachable; refusing to dispatch ungoverned tool call");
                        Ok(HookOutcome::reject(format!(
                            "ZP gate unreachable: {e}. Refusing to dispatch ungoverned tool call. \
                             Check zp-server is running (zp serve --port 17010), then retry."
                        )))
                    }
                }
            }

            HookEvent::ResponseTransform {
                user_id,
                thread_id,
                response,
            } => {
                // Try the resolved-thread key first, then fall back to the
                // None-thread key. BeforeInbound caches whatever thread_id the
                // channel layer provides; for many channels (web gateway, REPL)
                // the thread isn't resolved until the agent loop creates one,
                // so the inbound side caches under (user_id, None). The
                // BeforeToolCall arm above migrates (user_id, None) →
                // (user_id, Some(thread_id)) when a tool fires, but turns
                // without tool calls never trigger that migration — so the
                // fallback below is required for text-only turns.
                let key_resolved = (user_id.clone(), Some(thread_id.clone()));
                let key_none = (user_id.clone(), None);
                let user_input = {
                    let mut cache = self.last_user_input.write().await;
                    cache
                        .remove(&key_resolved)
                        .or_else(|| cache.remove(&key_none))
                };
                // SAFETY: chain_parent_receipt_id is a heuristic, not authoritative.
                // Single-tool turns: the only gate, exact parent. Multi-tool turns:
                // the closest-in-time predecessor (last gate). The rigorous
                // correlation keys for "what observation came from which run" are
                // thread_id and run_id — Reflector queries should group by those,
                // not walk parent chains.
                let receipt = self.last_gate_receipt.write().await.remove(&key_resolved);

                let Some(input) = user_input else {
                    // No matching BeforeInbound was seen for this thread (shouldn't
                    // happen in normal flow). Skip the observation rather than
                    // emit a half-formed pair.
                    return Ok(HookOutcome::ok());
                };

                match self
                    .client
                    .observe(&input, response, receipt.as_deref())
                    .await
                {
                    Ok(()) => {}
                    Err(e @ ZpError::Auth { .. }) => {
                        let structural = e.is_structural_auth();
                        let (status, reason) = match &e {
                            ZpError::Auth { status, reason } => (*status, reason.clone()),
                            _ => unreachable!(),
                        };
                        if structural {
                            tracing::warn!(
                                status,
                                reason = %reason,
                                "zp observation envelope rejected (structural); disabling hook for session"
                            );
                            self.disable();
                        } else {
                            tracing::debug!(
                                status,
                                reason = %reason,
                                "zp observation envelope rejected (transient); not disabling"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "zp observation emit failed; not blocking turn");
                    }
                }
                Ok(HookOutcome::ok())
            }

            // Other hook points are not relevant to cognition governance.
            _ => Ok(HookOutcome::ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_hash_is_deterministic() {
        let v = serde_json::json!({ "a": 1, "b": "hello" });
        let h1 = ZpHook::args_hash(&v);
        let h2 = ZpHook::args_hash(&v);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // blake3 hex = 32 bytes * 2
    }

    #[test]
    fn args_hash_differs_for_different_inputs() {
        let a = ZpHook::args_hash(&serde_json::json!({"x": 1}));
        let b = ZpHook::args_hash(&serde_json::json!({"x": 2}));
        assert_ne!(a, b);
    }
}
