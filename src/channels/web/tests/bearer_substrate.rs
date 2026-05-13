//! Tests for the substrate-session cookie auth path (acceptance criteria #138).
//!
//! Unit tests for SubstrateSessionVerifier + integration tests that drive
//! auth_middleware end-to-end, mirroring the shape of multi_tenant.rs.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header::COOKIE};
use axum::middleware;
use axum::routing::get;
use tower::ServiceExt;

use crate::channels::web::auth::{AuthenticatedUser, CombinedAuthState, SubstrateSessionVerifier, auth_middleware};
use crate::channels::web::sse::DEFAULT_BROADCAST_BUFFER;
use crate::config::GatewayConfig;

const TEST_KEY: &str = "test-signing-key-32-bytes-long!!";
const COOKIE_NAME: &str = "zp_session";

// ── Token builder ────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn make_token(key: &str, sub: &str, exp_offset_ms: i64) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let now = now_ms();
    let payload = serde_json::json!({
        "sub": sub,
        "name": "Test Operator",
        "cap": "[\"workspace:admin\"]",
        "iat": now,
        "exp": now + exp_offset_ms,
    });
    let payload_b64 =
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap().as_bytes());
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).unwrap();
    mac.update(payload_b64.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload_b64}.{sig}")
}

fn verifier() -> SubstrateSessionVerifier {
    SubstrateSessionVerifier::new(TEST_KEY, COOKIE_NAME.to_string())
}

// ── Unit tests: SubstrateSessionVerifier ─────────────────────────────────────

#[test]
fn valid_token_verifies() {
    let claims = verifier()
        .verify(&make_token(TEST_KEY, "ken", 60_000))
        .expect("should verify");
    assert_eq!(claims.operator_id, "ken");
    assert_eq!(claims.operator_name, "Test Operator");
    assert_eq!(claims.capabilities, vec!["workspace:admin"]);
}

#[test]
fn wrong_key_is_rejected() {
    let bad_tok = make_token("wrong-key-32-bytes-minimum-len!!", "ken", 60_000);
    assert!(verifier().verify(&bad_tok).is_err(), "wrong key must be rejected");
}

#[test]
fn expired_token_is_rejected() {
    let tok = make_token(TEST_KEY, "ken", -60_000); // 1 min in the past
    let err = verifier().verify(&tok).unwrap_err();
    assert_eq!(err, "token expired");
}

#[test]
fn malformed_no_dot_is_rejected() {
    assert!(verifier().verify("nodothere").is_err());
}

#[test]
fn malformed_bad_base64_is_rejected() {
    assert!(verifier().verify("!!!.===").is_err());
}

#[test]
fn tampered_payload_rejected() {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let good = make_token(TEST_KEY, "ken", 60_000);
    let sig = &good[good.find('.').unwrap() + 1..];
    let evil_payload = URL_SAFE_NO_PAD.encode(
        serde_json::json!({
            "sub": "admin", "name": "Evil", "cap": "[]",
            "iat": 0, "exp": i64::MAX
        })
        .to_string()
        .as_bytes(),
    );
    let tampered = format!("{evil_payload}.{sig}");
    assert!(verifier().verify(&tampered).is_err(), "tampered payload must be rejected");
}

// ── Integration helpers ───────────────────────────────────────────────────────

fn auth_state_with_substrate() -> CombinedAuthState {
    let mut s = CombinedAuthState::from(crate::channels::web::auth::MultiAuthState::empty());
    s.substrate_session = Some(SubstrateSessionVerifier::new(
        TEST_KEY,
        COOKIE_NAME.to_string(),
    ));
    s
}

fn auth_state_bearer_only(token: &str) -> CombinedAuthState {
    CombinedAuthState::from(crate::channels::web::auth::MultiAuthState::single(
        token.to_string(),
        "owner".to_string(),
    ))
}

/// Minimal protected app that returns the authenticated user_id as plain text.
fn protected_app(auth: CombinedAuthState) -> Router {
    Router::new()
        .route(
            "/test",
            get(|u: AuthenticatedUser| async move { u.0.user_id }),
        )
        .layer(middleware::from_fn_with_state(auth, auth_middleware))
}

async fn status(app: &Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

async fn body_text(app: &Router, req: Request<Body>) -> String {
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn get_with_cookie(cookie: &str) -> Request<Body> {
    Request::builder()
        .uri("/test")
        .header(COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

fn get_no_auth() -> Request<Body> {
    Request::builder().uri("/test").body(Body::empty()).unwrap()
}

// ── Integration: substrate-session enabled ────────────────────────────────────

#[tokio::test]
async fn valid_cookie_authenticates() {
    let app = protected_app(auth_state_with_substrate());
    let token = make_token(TEST_KEY, "ken", 60_000);
    let user_id = body_text(&app, get_with_cookie(&format!("{COOKIE_NAME}={token}"))).await;
    assert_eq!(user_id, "ken");
}

#[tokio::test]
async fn invalid_signature_is_401() {
    let app = protected_app(auth_state_with_substrate());
    let bad = make_token("wrong-key-32-bytes-minimum-len!!", "ken", 60_000);
    assert_eq!(
        status(&app, get_with_cookie(&format!("{COOKIE_NAME}={bad}"))).await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn expired_cookie_is_401() {
    let app = protected_app(auth_state_with_substrate());
    let tok = make_token(TEST_KEY, "ken", -60_000);
    assert_eq!(
        status(&app, get_with_cookie(&format!("{COOKIE_NAME}={tok}"))).await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn no_cookie_is_401_when_no_other_auth() {
    let app = protected_app(auth_state_with_substrate());
    assert_eq!(status(&app, get_no_auth()).await, StatusCode::UNAUTHORIZED);
}

// ── Integration: disabled falls through to bearer ────────────────────────────

#[tokio::test]
async fn disabled_substrate_session_ignores_cookie_and_uses_bearer() {
    // substrate_session disabled — CombinedAuthState has bearer only.
    let app = protected_app(auth_state_bearer_only("explicit-bearer"));
    // Cookie present but substrate_session is off → ignored; bearer succeeds.
    let req = Request::builder()
        .uri("/test")
        .header("Authorization", "Bearer explicit-bearer")
        .header(COOKIE, format!("{COOKIE_NAME}=whatever"))
        .body(Body::empty())
        .unwrap();
    let user_id = body_text(&app, req).await;
    assert_eq!(user_id, "owner");
}

// ── GatewayConfig: acceptance criterion #1 — no regression when disabled ─────

#[test]
fn disabled_substrate_session_leaves_gateway_behavior_unchanged() {
    use crate::channels::web::GatewayChannel;
    let config = GatewayConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        auth_token: Some("tok-abc".to_string()),
        max_connections: 16,
        broadcast_buffer: DEFAULT_BROADCAST_BUFFER,
        workspace_read_scopes: vec![],
        memory_layers: vec![],
        oidc: None,
        substrate_session: None,
    };
    let gw = GatewayChannel::new(config, "owner".to_string());
    // Bearer token must still be registered as before.
    assert_eq!(gw.auth_token(), "tok-abc");
}
