//! Regression for principle #4 (convergent paths, convergent observability)
//! and failure mode F5 in `OBSERVABILITY-2026-05.md`: when OIDC is the
//! configured primary auth path, no auto-generated bearer should be
//! registered. The SPA's stale localStorage token must NOT succeed
//! without OIDC validation.

use crate::channels::web::GatewayChannel;
use crate::channels::web::sse::DEFAULT_BROADCAST_BUFFER;
use crate::config::{GatewayConfig, GatewayOidcConfig};

fn base_config(auth_token: Option<String>, oidc: Option<GatewayOidcConfig>) -> GatewayConfig {
    GatewayConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        auth_token,
        max_connections: 16,
        broadcast_buffer: DEFAULT_BROADCAST_BUFFER,
        workspace_read_scopes: vec![],
        memory_layers: vec![],
        oidc,
        substrate_session: None,
    }
}

fn oidc_config() -> GatewayOidcConfig {
    GatewayOidcConfig {
        header: "cf-access-jwt-assertion".to_string(),
        jwks_url: "https://example.cloudflareaccess.com/cdn-cgi/access/certs".to_string(),
        issuer: Some("https://example.cloudflareaccess.com".to_string()),
        audience: Some("deadbeef".to_string()),
    }
}

#[tokio::test]
async fn oidc_active_disables_bearer_auto_gen() {
    let config = base_config(None, Some(oidc_config()));
    let gw = GatewayChannel::new(config, "owner".to_string());
    assert_eq!(
        gw.auth_token(),
        "",
        "OIDC-active gateway must not register an auto-generated bearer"
    );
}

#[tokio::test]
async fn explicit_bearer_coexists_with_oidc() {
    // Transitional: an operator may want both alive while migrating away
    // from a long-lived bearer.
    let config = base_config(Some("explicit-tok".to_string()), Some(oidc_config()));
    let gw = GatewayChannel::new(config, "owner".to_string());
    assert_eq!(
        gw.auth_token(),
        "explicit-tok",
        "explicit GATEWAY_AUTH_TOKEN must still register the bearer when OIDC is also on"
    );
}

#[tokio::test]
async fn no_oidc_no_token_auto_generates_bearer() {
    // Preserve the standalone-developer path: bearer auto-gen when
    // neither an explicit token nor OIDC is configured.
    let config = base_config(None, None);
    let gw = GatewayChannel::new(config, "owner".to_string());
    let tok = gw.auth_token();
    assert!(
        !tok.is_empty(),
        "without OIDC, missing GATEWAY_AUTH_TOKEN must trigger auto-gen"
    );
    assert_eq!(tok.len(), 64, "auto-gen token is 32 bytes hex-encoded");
}
