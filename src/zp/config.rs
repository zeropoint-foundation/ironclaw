//! ZP integration config from environment.

use secrecy::SecretString;

use crate::config::helpers::{optional_env, parse_bool_env};
use crate::error::ConfigError;

/// Default ZP server base URL when `ZP_BASE_URL` is unset.
pub const DEFAULT_ZP_BASE_URL: &str = "http://localhost:17010";

/// Default agent-name field sent on gate calls when `ZP_AGENT_NAME` is unset.
pub const DEFAULT_ZP_AGENT_NAME: &str = "ironclaw";

/// Resolved ZP integration configuration.
///
/// Only constructed when `IRONCLAW_ZP_ENABLED=true` and required credentials
/// are present. Otherwise [`ZpConfig::from_env`] returns `Ok(None)` and the
/// hook is never registered.
#[derive(Debug, Clone)]
pub struct ZpConfig {
    /// ZP server base URL (no trailing slash).
    pub base_url: String,
    /// Bearer token for `Authorization: Bearer <token>`.
    pub session_token: SecretString,
    /// Value for the `agent` field on gate-call requests.
    pub agent_name: String,
}

impl ZpConfig {
    /// Resolve from environment.
    ///
    /// Returns `Ok(None)` if `IRONCLAW_ZP_ENABLED` is unset, false, or if
    /// `ZP_SESSION_TOKEN` is missing while enabled (logs a warning and
    /// remains disabled — the binary still boots).
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        if !parse_bool_env("IRONCLAW_ZP_ENABLED", false)? {
            return Ok(None);
        }

        let Some(token) = optional_env("ZP_SESSION_TOKEN")?.filter(|t| !t.is_empty()) else {
            tracing::warn!(
                "IRONCLAW_ZP_ENABLED=true but ZP_SESSION_TOKEN is unset; \
                 ZP cognition-governance integration disabled"
            );
            return Ok(None);
        };

        let base_url = optional_env("ZP_BASE_URL")?
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_ZP_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let agent_name = optional_env("ZP_AGENT_NAME")?
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| DEFAULT_ZP_AGENT_NAME.to_string());

        Ok(Some(Self {
            base_url,
            session_token: SecretString::from(token),
            agent_name,
        }))
    }
}
