//! ZeroPoint cognition-governance integration (outbound).
//!
//! IronClaw is a ZP-governed tool. When `IRONCLAW_ZP_ENABLED=true`, the
//! agent loop calls ZP's gate before invoking any tool and emits a
//! cognition observation after each assistant turn completes. Wiring is
//! a single [`Hook`] registered at startup; standalone runs (the env
//! var unset or `false`) are zero-overhead.
//!
//! - Gate: `POST /api/v1/gate/tool-call`  — sends `tool_name + args_hash`
//!   and a few correlation keys; blocks the tool call on `allowed=false`.
//! - Observation: `POST /api/v1/cognition/observe` — sends the
//!   `[user, assistant]` message pair after the turn, parented to the
//!   most recent gate receipt for the thread (heuristic; see `hook.rs`).
//!
//! Failure mode is degrade-open: ZP transport / 5xx / timeout failures
//! never block the agent loop. Auth failures (401/403) disable the hook
//! for the rest of the session after a single warn-log.
//!
//! [`Hook`]: crate::hooks::Hook

pub mod client;
pub mod config;
pub mod hook;

pub use client::{GateDecision, ZpClient, ZpError};
pub use config::ZpConfig;
pub use hook::ZpHook;

use std::path::Path;
use std::sync::Arc;

use ed25519_dalek::SigningKey;

/// Bootstrap the Genesis-derived gate signer.
///
/// Composes with zeropoint task #152 (singular sovereign root): loads the
/// Genesis secret via `zp_keys::load_sovereign_root` (the canonical loader)
/// and derives the gate-request signer seed via
/// `zp_keys::derive_gate_signer_seed`. Same Genesis → same signer →
/// same kid the substrate's gate verifier expects.
///
/// Returns a `ZpError::Transport` on failure with the underlying message,
/// since callers surface the error into the same warn-log path as transport
/// failures during ZpClient construction.
pub fn bootstrap_gate_signer(genesis_record_path: &Path) -> Result<Arc<SigningKey>, ZpError> {
    let genesis = zp_keys::load_sovereign_root(genesis_record_path)
        .map_err(|e| ZpError::Transport(format!("load sovereign root: {}", e)))?;
    let seed = zp_keys::derive_gate_signer_seed(genesis);
    Ok(Arc::new(SigningKey::from_bytes(&seed)))
}
