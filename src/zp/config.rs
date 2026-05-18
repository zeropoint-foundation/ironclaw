//! ZP integration config from environment.

use std::path::PathBuf;

use crate::config::helpers::{optional_env, parse_bool_env};
use crate::error::ConfigError;

/// Default ZP server base URL when `ZP_BASE_URL` is unset.
pub const DEFAULT_ZP_BASE_URL: &str = "http://localhost:17010";

/// Default agent-name field sent on gate calls when `ZP_AGENT_NAME` is unset.
pub const DEFAULT_ZP_AGENT_NAME: &str = "ironclaw";

/// Default Genesis record path when `IRONCLAW_ZP_GENESIS_PATH` is unset.
/// Matches the substrate's `zp_paths::genesis_record_path()` resolution: the
/// operator's `~/ZeroPoint/genesis.json`. IronClaw never reads the file's
/// contents; it only passes the path to `zp_keys::load_sovereign_root`, which
/// is the singular sovereign-root loader.
fn default_genesis_record_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("ZeroPoint").join("genesis.json"))
        .unwrap_or_else(|| PathBuf::from("ZeroPoint/genesis.json"))
}

/// Resolved ZP integration configuration.
///
/// Only constructed when `IRONCLAW_ZP_ENABLED=true`. Otherwise
/// [`ZpConfig::from_env`] returns `Ok(None)` and the hook is never registered.
///
/// As of the genesis-signed-gate-requests migration (zeropoint design doc
/// `docs/handoffs/genesis-signed-gate-requests-design-2026-05.md`), per-request
/// authentication is a Genesis-derived Ed25519 envelope (`Authorization:
/// ZP-Sig …`). The legacy `ZP_SESSION_TOKEN` bearer is no longer read by
/// IronClaw — the gate signer is derived from the Genesis secret via
/// `zp_keys::derive_gate_signer_seed`, which produces the same key on both
/// sides of every gate call.
#[derive(Debug, Clone)]
pub struct ZpConfig {
    /// ZP server base URL (no trailing slash).
    pub base_url: String,
    /// Value for the `agent` field on gate-call requests.
    pub agent_name: String,
    /// Path to `genesis.json`. Passed to `zp_keys::load_sovereign_root` to
    /// load (or re-derive) the operator's Genesis secret. IronClaw never
    /// reads the file's contents itself — same discipline every other
    /// Genesis consumer in the substrate uses.
    pub genesis_record_path: PathBuf,
}

impl ZpConfig {
    /// Resolve from environment.
    ///
    /// Returns `Ok(None)` if `IRONCLAW_ZP_ENABLED` is unset or false. When
    /// enabled, the configuration always resolves (Genesis path defaults to
    /// `~/ZeroPoint/genesis.json`); failure to actually load the sovereign
    /// root is reported later, by the signer bootstrap, with an actionable
    /// error message.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        if !parse_bool_env("IRONCLAW_ZP_ENABLED", false)? {
            return Ok(None);
        }

        let base_url = optional_env("ZP_BASE_URL")?
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_ZP_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let agent_name = optional_env("ZP_AGENT_NAME")?
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| DEFAULT_ZP_AGENT_NAME.to_string());

        let genesis_record_path = optional_env("IRONCLAW_ZP_GENESIS_PATH")?
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_genesis_record_path);

        Ok(Some(Self {
            base_url,
            agent_name,
            genesis_record_path,
        }))
    }
}
