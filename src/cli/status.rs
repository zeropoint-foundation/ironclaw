//! System health and diagnostics CLI command.
//!
//! Checks database connectivity, session validity, embeddings,
//! WASM runtime, tool count, and channel availability.
//!
//! The `--runtime` flag produces the resolved view a running gateway
//! would use — implements principles #3 (state should be queryable) and
//! #6 (boot banner is the security posture) from
//! `docs/OBSERVABILITY-2026-05.md`.

use std::path::PathBuf;

use clap::Args;

use crate::boot_screen::{AuthPosture, OidcPosture};
use crate::bootstrap::ironclaw_base_dir;
use crate::cli::fmt;
use crate::settings::Settings;

/// Arguments for the `ironclaw status` subcommand.
#[derive(Args, Debug, Clone)]
pub struct StatusCommand {
    /// Show the resolved runtime view (auth posture, full config) rather
    /// than the static config-file view. Mirrors what a running gateway
    /// would use.
    #[arg(long)]
    pub runtime: bool,
}

/// Load settings from JSON and TOML config files, matching the runtime
/// priority: TOML overlay > settings.json > defaults.
///
/// This mirrors the loading chain in `Config::from_env_with_toml()` but
/// without resolving the full `Config` (which requires async + secrets).
fn load_settings() -> Settings {
    load_settings_from(&Settings::default_path(), &Settings::default_toml_path())
}

/// Inner implementation with injectable paths (testable).
fn load_settings_from(json_path: &std::path::Path, toml_path: &std::path::Path) -> Settings {
    let mut settings = Settings::load_from(json_path);

    match Settings::load_toml(toml_path) {
        Ok(Some(toml_settings)) => {
            settings.merge_from(&toml_settings);
        }
        Ok(None) => {} // File not found — fine for default path
        Err(e) => {
            eprintln!("Warning: failed to parse {}: {}", toml_path.display(), e);
        }
    }

    settings
}

async fn load_acp_agents_for_status()
-> Result<crate::config::acp::AcpAgentsFile, crate::config::acp::AcpConfigError> {
    match crate::config::Config::from_env().await {
        Ok(config) => {
            let db: Option<std::sync::Arc<dyn crate::db::Database>> =
                crate::db::connect_from_config(&config.database)
                    .await
                    .ok()
                    .map(|db| db as std::sync::Arc<dyn crate::db::Database>);
            crate::config::acp::load_acp_agents_for_user(db.as_deref(), &config.owner_id).await
        }
        Err(_) => crate::config::acp::load_acp_agents().await,
    }
}

/// Run the status command, printing system health info.
pub async fn run_status_command() -> anyhow::Result<()> {
    let settings = load_settings();

    println!();
    println!("  {}IronClaw Status{}", fmt::bold(), fmt::reset());
    println!();

    // Version
    println!(
        "{}",
        fmt::kv_line(
            "Version",
            &format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            12,
        )
    );

    // Database
    let db_backend = std::env::var("DATABASE_BACKEND")
        .ok()
        .unwrap_or_else(|| "postgres".to_string());
    let db_value = match db_backend.as_str() {
        "libsql" | "turso" | "sqlite" => {
            let path = std::env::var("LIBSQL_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| crate::config::default_libsql_path());
            if path.exists() {
                let turso = if std::env::var("LIBSQL_URL").is_ok() {
                    " + Turso sync"
                } else {
                    ""
                };
                format!("libSQL ({}{})", path.display(), turso)
            } else {
                format!("libSQL (file missing: {})", path.display())
            }
        }
        _ => {
            if std::env::var("DATABASE_URL").is_ok() {
                match check_database().await {
                    Ok(()) => "connected (PostgreSQL)".to_string(),
                    Err(e) => format!("error ({})", e),
                }
            } else {
                "not configured".to_string()
            }
        }
    };
    println!("{}", fmt::kv_line("Database", &db_value, 12));

    // Session / Auth
    let session_path = crate::config::llm::default_session_path();
    let session_value = if session_path.exists() {
        format!("found ({})", session_path.display())
    } else {
        "not found (run `ironclaw onboard`)".to_string()
    };
    println!("{}", fmt::kv_line("Session", &session_value, 12));

    // Secrets (auto-detect from env only; skip keychain probe to avoid
    // triggering macOS system password dialogs on a simple status check)
    let secrets_value = if std::env::var("SECRETS_MASTER_KEY").is_ok() {
        "configured (env)".to_string()
    } else {
        // We don't probe the keychain here because get_generic_password()
        // triggers macOS unlock+authorization dialogs, which is bad UX for
        // a read-only status command. If onboarding completed with keychain
        // storage, the key is there; we just can't cheaply verify it.
        "env not set (keychain may be configured)".to_string()
    };
    println!("{}", fmt::kv_line("Secrets", &secrets_value, 12));

    // Embeddings
    let emb_enabled = settings.embeddings.enabled
        || std::env::var("OPENAI_API_KEY").is_ok()
        || std::env::var("EMBEDDING_ENABLED")
            .map(|v| v == "true")
            .unwrap_or(false);
    let emb_value = if emb_enabled {
        format!(
            "enabled (provider: {}, model: {})",
            settings.embeddings.provider, settings.embeddings.model
        )
    } else {
        "disabled".to_string()
    };
    println!("{}", fmt::kv_line("Embeddings", &emb_value, 12));

    // WASM tools
    let tools_dir = settings
        .wasm
        .tools_dir
        .clone()
        .unwrap_or_else(default_tools_dir);
    let tools_value = if tools_dir.exists() {
        let count = count_wasm_files(&tools_dir);
        format!("{} installed ({})", count, tools_dir.display())
    } else {
        format!("directory not found ({})", tools_dir.display())
    };
    println!("{}", fmt::kv_line("WASM Tools", &tools_value, 12));

    // WASM channels
    let channels_dir = settings
        .channels
        .wasm_channels_dir
        .clone()
        .unwrap_or_else(default_channels_dir);
    let mut channel_info = vec!["cli".to_string()];
    if settings.channels.http_enabled {
        channel_info.push(format!(
            "http:{}",
            settings.channels.http_port.unwrap_or(3000)
        ));
    }
    if let Some(wasm_summary) = format_wasm_channels_summary(&settings, &channels_dir) {
        channel_info.push(wasm_summary);
    }
    println!("{}", fmt::kv_line("Channels", &channel_info.join(", "), 12));

    // Heartbeat
    let hb_enabled = settings.heartbeat.enabled
        || std::env::var("HEARTBEAT_ENABLED")
            .map(|v| v == "true")
            .unwrap_or(false);
    let hb_value = if hb_enabled {
        format!("enabled (interval: {}s)", settings.heartbeat.interval_secs)
    } else {
        "disabled".to_string()
    };
    println!("{}", fmt::kv_line("Heartbeat", &hb_value, 12));

    // MCP servers
    let mcp_value = match crate::tools::mcp::config::load_mcp_servers().await {
        Ok(servers) => {
            let enabled = servers.servers.iter().filter(|s| s.enabled).count();
            let total = servers.servers.len();
            format!("{} enabled / {} configured", enabled, total)
        }
        Err(_) => "none configured".to_string(),
    };
    println!("{}", fmt::kv_line("MCP Servers", &mcp_value, 12));

    // ACP agents
    let acp_value = match load_acp_agents_for_status().await {
        Ok(agents) => {
            let enabled = agents.agents.iter().filter(|a| a.enabled).count();
            let total = agents.agents.len();
            format!("{} enabled / {} configured", enabled, total)
        }
        Err(_) => "none configured".to_string(),
    };
    println!("{}", fmt::kv_line("ACP Agents", &acp_value, 12));

    // Config path
    println!();
    println!(
        "{}",
        fmt::kv_line(
            "Config",
            &crate::bootstrap::ironclaw_env_path().display().to_string(),
            12,
        )
    );

    Ok(())
}

#[cfg(feature = "postgres")]
async fn check_database() -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL not set"))?;

    let config: deadpool_postgres::Config = deadpool_postgres::Config {
        url: Some(url),
        ..Default::default()
    };
    let pool = crate::db::tls::create_pool(&config, crate::config::SslMode::from_env())
        .map_err(|e| anyhow::anyhow!("pool error: {}", e))?;

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), pool.get())
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    client
        .execute("SELECT 1", &[])
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(())
}

#[cfg(not(feature = "postgres"))]
async fn check_database() -> anyhow::Result<()> {
    // For non-postgres backends, just report configured
    Ok(())
}

fn count_wasm_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "wasm"))
                .count()
        })
        .unwrap_or(0)
}

fn format_wasm_channels_summary(
    settings: &Settings,
    channels_dir: &std::path::Path,
) -> Option<String> {
    if settings.channels.wasm_channels_enabled && !settings.channels.wasm_channels.is_empty() {
        let mut channels = settings.channels.wasm_channels.clone();
        channels.sort();
        return Some(format!("{} wasm ({})", channels.len(), channels.join(", ")));
    }

    let wasm_count = count_wasm_files(channels_dir);
    if wasm_count > 0 {
        if settings.channels.wasm_channels_enabled {
            return Some(format!("{} wasm", wasm_count));
        } else {
            return Some(format!(
                "{} wasm installed ({})",
                wasm_count,
                channels_dir.display()
            ));
        }
    }

    None
}

fn default_tools_dir() -> PathBuf {
    ironclaw_base_dir().join("tools")
}

fn default_channels_dir() -> PathBuf {
    ironclaw_base_dir().join("channels")
}

/// Render the `--runtime` view: the resolved config a running gateway
/// would use, plus the auth posture inferred from that config.
///
/// Implements principles #3 and #6 from `OBSERVABILITY-2026-05.md`.
/// Closes failure mode F4 (boot banner doesn't reflect auth posture).
pub async fn run_runtime_status_command() -> anyhow::Result<()> {
    let config = crate::config::Config::from_env()
        .await
        .map_err(|e| anyhow::anyhow!("config resolve: {e}"))?;

    println!();
    println!("  {}IronClaw Status (runtime){}", fmt::bold(), fmt::reset());
    println!();

    // Version + PID + uptime
    println!(
        "{}",
        fmt::kv_line("Version", &format!("v{}", env!("CARGO_PKG_VERSION")), 14,)
    );

    if let Some((pid, uptime)) = read_pid_and_uptime() {
        println!(
            "{}",
            fmt::kv_line("Daemon PID", &format!("{pid} (up {uptime})"), 14)
        );
    } else {
        println!(
            "{}",
            fmt::kv_line("Daemon PID", "not running (no pid lock)", 14)
        );
    }

    // CLI mode + model
    let cli_mode = if config.channels.tui.is_some() {
        "tui"
    } else if config.channels.cli.enabled {
        "repl"
    } else {
        "headless"
    };
    let logs_note = if cli_mode == "tui" {
        " (logs at ~/.ironclaw/logs/)"
    } else {
        ""
    };
    println!(
        "{}",
        fmt::kv_line("CLI mode", &format!("{cli_mode}{logs_note}"), 14)
    );

    // Model: pulled from settings rather than the per-provider config
    // subtree — the latter is structurally provider-specific and not
    // worth a runtime lookup for a status print.
    let settings = load_settings();
    let model_display = settings
        .selected_model
        .clone()
        .unwrap_or_else(|| "(per-backend default)".to_string());
    println!(
        "{}",
        fmt::kv_line(
            "Model",
            &format!("{model_display} via {}", config.llm.backend),
            14,
        )
    );

    // Gateway
    if let Some(ref gateway) = config.channels.gateway {
        println!(
            "{}",
            fmt::kv_line(
                "Gateway",
                &format!("http://{}:{}/", gateway.host, gateway.port),
                14,
            )
        );
    } else {
        println!("{}", fmt::kv_line("Gateway", "disabled", 14));
    }

    // Auth posture — the core surface of principle #6
    println!();
    println!("  {}Auth:{}", fmt::bold(), fmt::reset());
    if let Some(posture) = AuthPosture::from_config(&config) {
        print_auth_posture(&posture);
    } else {
        println!("    (gateway not configured)");
    }

    Ok(())
}

fn print_auth_posture(posture: &AuthPosture) {
    if let Some(ref oidc) = posture.oidc {
        println!("    OIDC:        enabled");
        print_oidc_details(oidc);
    } else {
        println!("    OIDC:        disabled");
    }
    let bearer_note = if posture.bearer_enabled {
        if posture.oidc.is_some() {
            "enabled (coexists with OIDC; see #116)"
        } else if std::env::var("GATEWAY_AUTH_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            "enabled (env-set)"
        } else {
            "enabled (auto-generated)"
        }
    } else {
        "disabled (OIDC primary)"
    };
    println!("    Bearer:      {bearer_note}");
}

fn print_oidc_details(oidc: &OidcPosture) {
    if let Some(ref issuer) = oidc.issuer {
        println!("      issuer:    {issuer}");
    }
    if let Some(ref audience) = oidc.audience {
        // Show a prefix only — full audience can be long and lock-screen
        // visible during pair programming. Operators who want the full
        // value can read it out of ~/.ironclaw/.env.
        let head: String = audience.chars().take(12).collect();
        let suffix = if audience.chars().count() > 12 {
            "..."
        } else {
            ""
        };
        println!("      audience:  {head}{suffix}");
    }
    println!("      header:    {}", oidc.header);
    println!("      jwks_url:  {}", oidc.jwks_url);
}

/// Read `~/.ironclaw/ironclaw.pid` and report its content plus a coarse
/// uptime derived from the file's mtime. Returns `None` when the PID
/// file is absent or unreadable.
///
/// Liveness is not checked here — a stale pid file from a crashed
/// daemon will display a stale PID. Operators querying `status
/// --runtime` should treat the PID as advisory; a hard liveness check
/// belongs in `ironclaw doctor` where the process-table dep is
/// already paid for.
fn read_pid_and_uptime() -> Option<(u32, String)> {
    let pid_path = crate::bootstrap::pid_lock_path();
    let pid: u32 = std::fs::read_to_string(&pid_path)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let mtime = std::fs::metadata(&pid_path).ok()?.modified().ok()?;
    let elapsed = mtime.elapsed().ok()?;
    Some((pid, format_uptime(elapsed)))
}

fn format_uptime(d: std::time::Duration) -> String {
    let total = d.as_secs();
    let hours = total / 3600;
    let mins = (total % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::{format_wasm_channels_summary, load_settings_from};
    use crate::settings::Settings;

    /// Regression test for #354: load_settings_from must read config.toml.
    #[test]
    fn reads_toml_heartbeat_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json_path = dir.path().join("settings.json");
        let toml_path = dir.path().join("config.toml");

        // No JSON file — only TOML
        std::fs::write(
            &toml_path,
            "[heartbeat]\nenabled = true\ninterval_secs = 600",
        )
        .expect("write toml");

        let settings = load_settings_from(&json_path, &toml_path);
        assert!(settings.heartbeat.enabled);
        assert_eq!(settings.heartbeat.interval_secs, 600);
    }

    /// Without any config files, defaults are returned.
    #[test]
    fn defaults_without_config_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = load_settings_from(
            &dir.path().join("nonexistent.json"),
            &dir.path().join("nonexistent.toml"),
        );
        assert!(!settings.heartbeat.enabled);
    }

    /// settings.json is respected.
    #[test]
    fn reads_json_heartbeat_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json_path = dir.path().join("settings.json");
        let toml_path = dir.path().join("nonexistent.toml");

        std::fs::write(
            &json_path,
            r#"{"heartbeat":{"enabled":true,"interval_secs":900}}"#,
        )
        .expect("write json");

        let settings = load_settings_from(&json_path, &toml_path);
        assert!(settings.heartbeat.enabled);
        assert_eq!(settings.heartbeat.interval_secs, 900);
    }

    /// TOML overlay wins over JSON settings.
    #[test]
    fn toml_overlay_wins_over_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json_path = dir.path().join("settings.json");
        let toml_path = dir.path().join("config.toml");

        std::fs::write(
            &json_path,
            r#"{"heartbeat":{"enabled":false,"interval_secs":100}}"#,
        )
        .expect("write json");
        std::fs::write(
            &toml_path,
            "[heartbeat]\nenabled = true\ninterval_secs = 200",
        )
        .expect("write toml");

        let settings = load_settings_from(&json_path, &toml_path);
        assert!(settings.heartbeat.enabled);
        assert_eq!(settings.heartbeat.interval_secs, 200);
    }

    /// Invalid TOML is warned but doesn't crash; falls back to JSON/defaults.
    #[test]
    fn invalid_toml_falls_back_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json_path = dir.path().join("settings.json");
        let toml_path = dir.path().join("config.toml");

        std::fs::write(
            &json_path,
            r#"{"heartbeat":{"enabled":true,"interval_secs":500}}"#,
        )
        .expect("write json");
        std::fs::write(&toml_path, "this is not valid toml [[[").expect("write bad toml");

        let settings = load_settings_from(&json_path, &toml_path);
        // Should fall back to JSON values, not crash
        assert!(settings.heartbeat.enabled);
        assert_eq!(settings.heartbeat.interval_secs, 500);
    }

    #[test]
    fn formats_enabled_wasm_channels_with_names() {
        let mut settings = Settings::default();
        settings.channels.wasm_channels_enabled = true;
        settings.channels.wasm_channels = vec!["telegram".to_string(), "slack".to_string()];
        let dir = tempfile::tempdir().expect("tempdir");

        let summary = format_wasm_channels_summary(&settings, dir.path());

        assert_eq!(summary.as_deref(), Some("2 wasm (slack, telegram)"));
    }

    #[test]
    fn formats_installed_wasm_channels_when_enabled_list_is_empty() {
        let mut settings = Settings::default();
        settings.channels.wasm_channels_enabled = false;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::File::create(dir.path().join("telegram.wasm")).expect("write wasm");
        std::fs::File::create(dir.path().join("slack.wasm")).expect("write wasm");

        let summary = format_wasm_channels_summary(&settings, dir.path());
        let expected = format!("2 wasm installed ({})", dir.path().display());

        assert_eq!(summary.as_deref(), Some(expected.as_str()));
    }
}
