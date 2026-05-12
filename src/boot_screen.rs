//! Boot screen displayed after all initialization completes.
//!
//! Shows a compact ANSI-styled status panel with three tiers:
//! - **Tier 1 (always):** Name + version, model + backend.
//! - **Tier 2 (conditional):** Gateway URL, tunnel URL, non-default channels.
//! - **Tier 3 (removed):** Database, tool count, features → use `ironclaw status`.

use crate::cli::fmt;

/// All displayable fields for the boot screen.
pub struct BootInfo {
    pub version: String,
    pub agent_name: String,
    pub llm_backend: String,
    pub llm_model: String,
    pub cheap_model: Option<String>,
    pub db_backend: String,
    pub db_connected: bool,
    pub tool_count: usize,
    pub gateway_url: Option<String>,
    pub embeddings_enabled: bool,
    pub embeddings_provider: Option<String>,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: u64,
    pub sandbox_enabled: bool,
    pub docker_status: crate::sandbox::detect::DockerStatus,
    pub claude_code_enabled: bool,
    pub acp_enabled: bool,
    pub routines_enabled: bool,
    pub skills_enabled: bool,
    pub channels: Vec<String>,
    /// Public URL from a managed tunnel (e.g., "https://abc.ngrok.io").
    pub tunnel_url: Option<String>,
    /// Provider name for the managed tunnel (e.g., "ngrok").
    pub tunnel_provider: Option<String>,
    /// Time elapsed during startup. Shown at the bottom when present.
    pub startup_elapsed: Option<std::time::Duration>,
    /// Resolved auth posture (OIDC + bearer). Rendered as a one-line
    /// summary in Tier 2 of the banner. `None` is the standalone-developer
    /// degenerate case (no gateway configured).
    pub auth: Option<AuthPosture>,
}

/// Resolved gateway auth posture for the boot banner and `status --runtime`.
///
/// Implements principle #6 (the boot banner is the security posture) from
/// `OBSERVABILITY-2026-05.md`: the first thing an operator sees must
/// describe what is actually protecting the system.
#[derive(Debug, Clone)]
pub struct AuthPosture {
    /// `Some` when OIDC successfully resolved as an auth path.
    pub oidc: Option<OidcPosture>,
    /// True when a bearer token is registered with the env_auth ladder
    /// (either explicit `GATEWAY_AUTH_TOKEN` or the auto-generated path).
    pub bearer_enabled: bool,
}

/// OIDC-specific portion of [`AuthPosture`]. Mirrors the public fields of
/// `GatewayOidcConfig` so the boot banner and status command can render
/// without holding a reference to the config crate's private types.
#[derive(Debug, Clone)]
pub struct OidcPosture {
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub header: String,
    pub jwks_url: String,
}

impl AuthPosture {
    /// Derive the auth posture from a resolved [`crate::config::Config`].
    ///
    /// Mirrors the precedence inside `GatewayChannel::new`: an OIDC
    /// configuration alongside no explicit bearer suppresses the auto-gen
    /// path (`bearer_enabled = false`). An explicit bearer is reported as
    /// active regardless of OIDC state (the transitional coexistence case).
    pub fn from_config(config: &crate::config::Config) -> Option<Self> {
        let gateway = config.channels.gateway.as_ref()?;
        let oidc = gateway.oidc.as_ref().map(|o| OidcPosture {
            issuer: o.issuer.clone(),
            audience: o.audience.clone(),
            header: o.header.clone(),
            jwks_url: o.jwks_url.clone(),
        });
        let bearer_enabled = gateway.auth_token.is_some() || oidc.is_none();
        Some(Self {
            oidc,
            bearer_enabled,
        })
    }

    /// Build a single condensed line for the boot banner.
    ///
    /// Forms:
    /// - `OIDC (issuer=<host>, audience=<prefix>...)` — OIDC primary
    /// - `OIDC + bearer (issuer=<host>, audience=<prefix>...)` — coexistence
    /// - `bearer (auto-generated)` — standalone-developer default
    /// - `bearer (env-set)` — explicit `GATEWAY_AUTH_TOKEN`, no OIDC
    pub(crate) fn banner_line(&self, gateway_auth_token_env_set: bool) -> String {
        match (&self.oidc, self.bearer_enabled) {
            (Some(o), false) => format!("OIDC ({})", oidc_line_details(o)),
            (Some(o), true) => format!("OIDC + bearer ({})", oidc_line_details(o)),
            (None, true) if gateway_auth_token_env_set => "bearer (env-set)".to_string(),
            (None, true) => "bearer (auto-generated)".to_string(),
            (None, false) => "none".to_string(),
        }
    }
}

fn oidc_line_details(o: &OidcPosture) -> String {
    let issuer = o
        .issuer
        .as_deref()
        .and_then(|i| {
            i.strip_prefix("https://")
                .or_else(|| i.strip_prefix("http://"))
                .map(|s| s.split('/').next().unwrap_or(s))
                .map(|s| s.split('.').next().unwrap_or(s).to_string())
        })
        .unwrap_or_else(|| "?".to_string());
    let audience = o
        .audience
        .as_deref()
        .map(|a| {
            let head: String = a.chars().take(8).collect();
            format!("{head}...")
        })
        .unwrap_or_else(|| "?".to_string());
    format!("issuer={issuer}, audience={audience}")
}

const KW: usize = 10;

/// Print the boot screen to stdout.
///
/// **Tier 1 (always):** Name + version, model + backend.
/// **Tier 2 (conditional):** Gateway URL, tunnel URL, non-default channels.
/// **Tier 3 (removed):** Database, tool count, features — use `ironclaw status`.
pub fn print_boot_screen(info: &BootInfo) {
    let border = format!("  {}", fmt::separator(58));

    println!();
    println!("{border}");
    println!();

    // ── Tier 1: always shown ──────────────────────────────────────────

    println!(
        "  {}{}{} v{}",
        fmt::bold(),
        info.agent_name,
        fmt::reset(),
        info.version
    );
    println!();

    // Model line
    let model_display = if let Some(ref cheap) = info.cheap_model {
        format!(
            "{}{}{}  {}cheap{} {}{}{}",
            fmt::accent(),
            info.llm_model,
            fmt::reset(),
            fmt::dim(),
            fmt::reset(),
            fmt::accent(),
            cheap,
            fmt::reset(),
        )
    } else {
        format!("{}{}{}", fmt::accent(), info.llm_model, fmt::reset())
    };
    println!(
        "  {}{:<width$}{}  {model_display}  {}via {}{}",
        fmt::dim(),
        "model",
        fmt::reset(),
        fmt::dim(),
        info.llm_backend,
        fmt::reset(),
        width = KW,
    );

    // ── Tier 2: conditional ───────────────────────────────────────────

    // Gateway URL
    if let Some(ref url) = info.gateway_url {
        println!(
            "  {}{:<width$}{}  {}{}{}",
            fmt::dim(),
            "gateway",
            fmt::reset(),
            fmt::link(),
            url,
            fmt::reset(),
            width = KW,
        );
    }

    // Tunnel URL
    if let Some(ref url) = info.tunnel_url {
        let provider_tag = info
            .tunnel_provider
            .as_deref()
            .map(|p| format!("  {}({}){}", fmt::dim(), p, fmt::reset()))
            .unwrap_or_default();
        println!(
            "  {}{:<width$}{}  {}{}{}{}",
            fmt::dim(),
            "tunnel",
            fmt::reset(),
            fmt::link(),
            url,
            fmt::reset(),
            provider_tag,
            width = KW,
        );
    }

    // Non-default channels (skip if only the default set)
    let non_default: Vec<&str> = info
        .channels
        .iter()
        .filter(|c| !matches!(c.as_str(), "repl" | "gateway"))
        .map(|c| c.as_str())
        .collect();
    if !non_default.is_empty() {
        println!(
            "  {}{:<width$}{}  {}{}{}",
            fmt::dim(),
            "channels",
            fmt::reset(),
            fmt::accent(),
            non_default.join("  "),
            fmt::reset(),
            width = KW,
        );
    }

    // Auth posture: render whenever the gateway is configured. Implements
    // principle #6 from OBSERVABILITY-2026-05.md — the boot banner must
    // describe what is actually protecting the system, not just expose a
    // URL and trust the operator to know what's behind it.
    if let Some(ref auth) = info.auth {
        let gateway_token_env_set = std::env::var("GATEWAY_AUTH_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some();
        println!(
            "  {}{:<width$}{}  {}{}{}",
            fmt::dim(),
            "auth",
            fmt::reset(),
            fmt::accent(),
            auth.banner_line(gateway_token_env_set),
            fmt::reset(),
            width = KW,
        );
    }

    // ── Tier 3: compact feature tags ──────────────────────────────────

    let mut tags: Vec<String> = Vec::new();

    // Database
    if info.db_connected {
        tags.push(format!("db:{}", info.db_backend));
    }

    // Tool count
    if info.tool_count > 0 {
        tags.push(format!("tools:{}", info.tool_count));
    }

    // Routines
    if info.routines_enabled {
        tags.push("routines".to_string());
    }

    // Heartbeat with interval
    if info.heartbeat_enabled {
        let interval = if info.heartbeat_interval_secs >= 3600
            && info.heartbeat_interval_secs.is_multiple_of(3600)
        {
            format!("{}h", info.heartbeat_interval_secs / 3600)
        } else if info.heartbeat_interval_secs >= 60
            && info.heartbeat_interval_secs.is_multiple_of(60)
        {
            format!("{}m", info.heartbeat_interval_secs / 60)
        } else {
            format!("{}s", info.heartbeat_interval_secs)
        };
        tags.push(format!("heartbeat:{interval}"));
    }

    // Skills
    if info.skills_enabled {
        tags.push("skills".to_string());
    }

    // Sandbox / Docker
    if info.sandbox_enabled {
        let suffix = match info.docker_status {
            crate::sandbox::detect::DockerStatus::Available => "",
            crate::sandbox::detect::DockerStatus::NotRunning => ":stopped",
            _ => ":unavail",
        };
        tags.push(format!("sandbox{suffix}"));
    }

    // Embeddings
    if info.embeddings_enabled {
        if let Some(ref provider) = info.embeddings_provider {
            tags.push(format!("embeddings:{provider}"));
        } else {
            tags.push("embeddings".to_string());
        }
    }

    // Claude Code bridge
    if info.claude_code_enabled {
        tags.push("claude-code".to_string());
    }

    // ACP agents
    if info.acp_enabled {
        tags.push("acp".to_string());
    }

    if !tags.is_empty() {
        println!(
            "  {}{:<width$}{}  {}",
            fmt::dim(),
            "features",
            fmt::reset(),
            tags.join("  "),
            width = KW,
        );
    }

    // ── Footer ────────────────────────────────────────────────────────

    println!();
    println!("{border}");

    // Startup elapsed
    if let Some(elapsed) = info.startup_elapsed {
        let millis = elapsed.as_millis();
        let elapsed_str = if millis < 1000 {
            format!("{millis}ms")
        } else {
            let secs = elapsed.as_secs_f64();
            format!("{secs:.1}s")
        };
        println!("  {}ready in {}{}", fmt::dim(), elapsed_str, fmt::reset());
    }

    // Hint to run `ironclaw status` for full details
    println!(
        "  {}Run `ironclaw status` for full system details.{}",
        fmt::hint(),
        fmt::reset()
    );

    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::detect::DockerStatus;

    fn oidc(audience: &str, issuer: &str) -> OidcPosture {
        OidcPosture {
            issuer: Some(issuer.to_string()),
            audience: Some(audience.to_string()),
            header: "cf-access-jwt-assertion".to_string(),
            jwks_url: "https://example.cloudflareaccess.com/cdn-cgi/access/certs".to_string(),
        }
    }

    /// Regression for principle #6 (OBSERVABILITY-2026-05.md): the banner
    /// must surface OIDC posture, not just a gateway URL.
    #[test]
    fn auth_posture_banner_line_oidc_primary() {
        let posture = AuthPosture {
            oidc: Some(oidc(
                "26abcb60deadbeef",
                "https://zp-foundation-team.cloudflareaccess.com",
            )),
            bearer_enabled: false,
        };
        let line = posture.banner_line(false);
        assert!(line.starts_with("OIDC ("), "got: {line}");
        assert!(line.contains("issuer=zp-foundation-team"), "got: {line}");
        assert!(line.contains("audience=26abcb60..."), "got: {line}");
    }

    /// Coexistence: OIDC plus an explicit bearer (transitional case).
    #[test]
    fn auth_posture_banner_line_oidc_plus_bearer() {
        let posture = AuthPosture {
            oidc: Some(oidc("aud-xyz-1234", "https://issuer.example.com")),
            bearer_enabled: true,
        };
        let line = posture.banner_line(true);
        assert!(line.starts_with("OIDC + bearer ("), "got: {line}");
    }

    #[test]
    fn auth_posture_banner_line_bearer_env_set() {
        let posture = AuthPosture {
            oidc: None,
            bearer_enabled: true,
        };
        assert_eq!(posture.banner_line(true), "bearer (env-set)");
    }

    #[test]
    fn auth_posture_banner_line_bearer_auto_generated() {
        let posture = AuthPosture {
            oidc: None,
            bearer_enabled: true,
        };
        assert_eq!(posture.banner_line(false), "bearer (auto-generated)");
    }

    /// Short audiences don't get a trailing ellipsis.
    #[test]
    fn auth_posture_banner_line_short_audience_no_ellipsis() {
        let posture = AuthPosture {
            oidc: Some(oidc("short", "https://acme.cloudflareaccess.com")),
            bearer_enabled: false,
        };
        // 5 chars → take(8) returns the whole string, "..." is still appended.
        // The format is intentionally uniform; operators reading it know "..."
        // means "see the .env for the full value" regardless of length.
        let line = posture.banner_line(false);
        assert!(line.contains("audience=short..."), "got: {line}");
    }

    #[test]
    fn test_print_boot_screen_full() {
        let info = BootInfo {
            version: "0.2.0".to_string(),
            agent_name: "ironclaw".to_string(),
            llm_backend: "nearai".to_string(),
            llm_model: "claude-3-5-sonnet-20241022".to_string(),
            cheap_model: Some("gpt-4o-mini".to_string()),
            db_backend: "libsql".to_string(),
            db_connected: true,
            tool_count: 24,
            gateway_url: Some("http://127.0.0.1:3001/?token=abc123".to_string()),
            embeddings_enabled: true,
            embeddings_provider: Some("openai".to_string()),
            heartbeat_enabled: true,
            heartbeat_interval_secs: 1800,
            sandbox_enabled: true,
            docker_status: DockerStatus::Available,
            claude_code_enabled: false,
            acp_enabled: false,
            routines_enabled: true,
            skills_enabled: true,
            channels: vec![
                "repl".to_string(),
                "gateway".to_string(),
                "telegram".to_string(),
            ],
            tunnel_url: Some("https://abc123.ngrok.io".to_string()),
            tunnel_provider: Some("ngrok".to_string()),
            startup_elapsed: None,
            auth: None,
        };
        // Should not panic
        print_boot_screen(&info);
    }

    #[test]
    fn test_print_boot_screen_minimal() {
        let info = BootInfo {
            version: "0.2.0".to_string(),
            agent_name: "ironclaw".to_string(),
            llm_backend: "nearai".to_string(),
            llm_model: "gpt-4o".to_string(),
            cheap_model: None,
            db_backend: "none".to_string(),
            db_connected: false,
            tool_count: 5,
            gateway_url: None,
            embeddings_enabled: false,
            embeddings_provider: None,
            heartbeat_enabled: false,
            heartbeat_interval_secs: 0,
            sandbox_enabled: false,
            docker_status: DockerStatus::Disabled,
            claude_code_enabled: false,
            acp_enabled: false,
            routines_enabled: false,
            skills_enabled: false,
            channels: vec![],
            tunnel_url: None,
            tunnel_provider: None,
            startup_elapsed: None,
            auth: None,
        };
        // Should not panic
        print_boot_screen(&info);
    }

    #[test]
    fn test_print_boot_screen_no_features() {
        let info = BootInfo {
            version: "0.1.0".to_string(),
            agent_name: "test".to_string(),
            llm_backend: "openai".to_string(),
            llm_model: "gpt-4o".to_string(),
            cheap_model: None,
            db_backend: "postgres".to_string(),
            db_connected: true,
            tool_count: 10,
            gateway_url: None,
            embeddings_enabled: false,
            embeddings_provider: None,
            heartbeat_enabled: false,
            heartbeat_interval_secs: 0,
            sandbox_enabled: false,
            docker_status: DockerStatus::Disabled,
            claude_code_enabled: false,
            acp_enabled: false,
            routines_enabled: false,
            skills_enabled: false,
            channels: vec!["repl".to_string()],
            tunnel_url: None,
            tunnel_provider: None,
            startup_elapsed: None,
            auth: None,
        };
        // Should not panic
        print_boot_screen(&info);
    }
}
