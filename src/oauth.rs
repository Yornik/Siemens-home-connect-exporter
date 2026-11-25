//! OAuth token management against the Home Connect token endpoint.
//!
//! Two flows live here: the background refresh of the access token, and
//! the one-time authorization that captures the first refresh token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::warn;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::metrics::Metrics;
use crate::stream::Budget;

/// The cached token stops being handed out this early. A call armed with
/// it then never races the server-side expiry.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The access token plus the instant it stops being trustworthy.
struct CachedAccess {
    token: String,
    expires_at: Instant,
}

/// Everything the mutex protects. The refresh token lives here because
/// Home Connect may rotate it on every call.
struct TokenState {
    access: Option<CachedAccess>,
    refresh: String,
}

/// The token endpoint's success payload.
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the access token expires.
    expires_in: u64,
    /// Present whenever the server rotates the refresh token.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The error object Home Connect can attach to any response, including a
/// 200.
#[derive(serde::Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    key: String,
    #[serde(default)]
    description: String,
}

/// A minimal view of any response body: only its error object matters
/// here.
#[derive(serde::Deserialize)]
struct ErrorProbe {
    #[serde(default)]
    error: Option<ApiErrorBody>,
}

/// Caches the Home Connect access token and refreshes it before it expires.
pub struct TokenManager {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    metrics: Arc<Metrics>,
    budget: Arc<Budget>,
    /// Guards the cached access token and the rotating refresh token.
    state: Mutex<TokenState>,
}

impl TokenManager {
    /// Creates a token manager for the configured credentials.
    ///
    /// Every refresh takes one request from the daily budget and counts
    /// `metrics.count_api_request("token")`.
    #[must_use]
    pub fn new(
        config: &Config,
        client: reqwest::Client,
        metrics: Arc<Metrics>,
        budget: Arc<Budget>,
    ) -> Self {
        Self {
            client,
            token_url: format!("{}/security/oauth/token", config.api_base),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            metrics,
            budget,
            state: Mutex::new(TokenState {
                access: None,
                refresh: config.refresh_token.clone(),
            }),
        }
    }

    /// Returns a valid access token.
    ///
    /// Refreshes when no token is cached or the cached one expires within
    /// 60 seconds. Home Connect may rotate the refresh token; the manager
    /// stores the new one and logs the change.
    ///
    /// # Errors
    /// Returns an error when no valid token is cached and the refresh call
    /// fails, or when the daily budget is spent.
    pub async fn get(&self) -> Result<String> {
        // One lock hold from the cache check through the refresh keeps
        // concurrent callers from racing into duplicate refresh calls.
        let mut state = self.state.lock().await;
        if let Some(token) = valid_access(&state) {
            return Ok(token);
        }
        self.refresh_locked(&mut state).await
    }

    /// Performs one refresh call. The caller already holds the state lock,
    /// so every concurrent caller shares this single in-flight refresh.
    async fn refresh_locked(&self, state: &mut TokenState) -> Result<String> {
        if !self.budget.take() {
            // Budget spent: keep serving whatever is still valid rather
            // than failing a call that has a working token.
            if let Some(token) = valid_access(state) {
                return Ok(token);
            }
            return Err(Error::Api {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "local daily budget spent".to_owned(),
            });
        }
        self.metrics.count_api_request("token");

        let response = match self
            .client
            .post(&self.token_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", state.refresh.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ]))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                self.metrics.count_api_error("token");
                return Err(Error::Http(err));
            }
        };

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.metrics.count_api_error("token");
            return Err(Error::RateLimited {
                retry_after_secs: retry_after_secs(response.headers()),
            });
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                self.metrics.count_api_error("token");
                return Err(Error::Http(err));
            }
        };
        if !status.is_success() {
            self.metrics.count_api_error("token");
            return Err(Error::Api {
                status,
                // Bodies can be huge; 200 chars are plenty to diagnose.
                body: body.chars().take(200).collect(),
            });
        }
        let parsed = match parse_token_body(&body, status) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.metrics.count_api_error("token");
                return Err(err);
            }
        };

        // Losing a rotated refresh token locks the account out, so the
        // replacement is stored before anything else happens.
        if let Some(rotated) = parsed.refresh_token
            && rotated != state.refresh
        {
            warn!("refresh token rotated");
            state.refresh = rotated;
        }

        let lifetime = Duration::from_secs(parsed.expires_in).saturating_sub(EXPIRY_MARGIN);
        // A hostile expires_in could overflow the clock; an immediately
        // stale cache is the safe fallback.
        let expires_at = Instant::now()
            .checked_add(lifetime)
            .unwrap_or_else(Instant::now);
        state.access = Some(CachedAccess {
            token: parsed.access_token.clone(),
            expires_at,
        });
        Ok(parsed.access_token)
    }
}

/// Runs the one-time authorization flow and returns the refresh token.
///
/// This is the only step with a human in the loop: print the URL, wait
/// for the redirect on `listen`, exchange the code.
///
/// # Errors
/// Returns an error when the redirect listener fails, when it stops
/// before a code arrives, or when the code exchange fails.
pub async fn authorize(
    client: &reqwest::Client,
    config: &Config,
    listen: SocketAddr,
) -> Result<String> {
    let redirect_uri = format!("http://{listen}/auth");
    // Bind before printing so the printed URL is guaranteed to answer.
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let url = authorize_url(&config.api_base, &config.client_id, &redirect_uri);
    // The portal application must carry exactly this redirect URI, or the
    // authorize page rejects the request before any login happens.
    tracing::info!(
        redirect_uri = %redirect_uri,
        "register this redirect URI on your Home Connect application"
    );
    // tracing rather than stdout: the exporter often runs headless.
    tracing::info!(url = %url, "open this URL in a browser to authorize the exporter");

    let (code_tx, mut code_rx) = mpsc::channel::<String>(1);
    let (done_tx, mut done_rx) = watch::channel(false);
    let app = axum::Router::new()
        .route("/auth", get(auth_redirect))
        .with_state(Arc::new(AuthListener {
            code: code_tx,
            done: done_tx,
        }));
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        // Ends once the redirect delivered a code; graceful shutdown then
        // lets the browser finish its confirmation page.
        let _ = done_rx.wait_for(|done| *done).await;
    });
    tokio::spawn(async move {
        // The task ends with the shutdown signal; its failures surface
        // through the code channel instead.
        let _ = server.await;
    });

    let code = code_rx.recv().await.ok_or_else(|| {
        Error::Io(std::io::Error::other(
            "the authorization listener stopped before a code arrived",
        ))
    })?;
    exchange_code(client, config, &code, &redirect_uri).await
}

/// Shares the authorization code between the redirect handler and the
/// flow waiting for it.
struct AuthListener {
    /// Delivers the code. A duplicate page reload just queues a second
    /// copy that nobody reads.
    code: mpsc::Sender<String>,
    /// Tells the HTTP server the flow is complete.
    done: watch::Sender<bool>,
}

/// Answers the OAuth redirect: hands the code over and confirms in plain
/// text.
async fn auth_redirect(
    State(listener): State<Arc<AuthListener>>,
    RawQuery(query): RawQuery,
) -> impl IntoResponse {
    let Some(code) = query.as_deref().and_then(query_code) else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain")],
            String::from("the redirect carried no authorization code; restart the flow"),
        );
    };
    // A send failure only means the flow already finished elsewhere.
    let _ = listener.code.send(code.clone()).await;
    let _ = listener.done.send(true);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain")],
        String::from("Authorization received. You can close this tab and return to the exporter."),
    )
}

/// Pulls the `code` value out of one raw query string.
fn query_code(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=')
            && name == "code"
        {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Reads one percent escape: `%xy` with two hex digits.
fn percent_byte(rest: &str) -> Option<(u8, &str)> {
    let body = rest.strip_prefix('%')?;
    let (digits, tail) = body.split_at_checked(2)?;
    let byte = u8::from_str_radix(digits, 16).ok()?;
    Some((byte, tail))
}

/// Decodes percent escapes and form-style pluses in one query value.
/// A malformed escape stays verbatim rather than corrupting the code.
fn percent_decode(value: &str) -> String {
    let mut decoded: Vec<u8> = Vec::with_capacity(value.len());
    let mut rest = value;
    while !rest.is_empty() {
        if let Some((byte, tail)) = percent_byte(rest) {
            decoded.push(byte);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix('+') {
            decoded.push(b' ');
            rest = tail;
        } else {
            // Any other character passes through unchanged.
            let head = rest.chars().next().unwrap_or_default();
            let mut buffer = [0u8; 4];
            decoded.extend_from_slice(head.encode_utf8(&mut buffer).as_bytes());
            rest = rest
                .split_at_checked(head.len_utf8())
                .map_or("", |(_, tail)| tail);
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Exchanges the authorization code for the account's first refresh
/// token.
async fn exchange_code(
    client: &reqwest::Client,
    config: &Config,
    code: &str,
    redirect_uri: &str,
) -> Result<String> {
    let response = client
        .post(format!("{}/security/oauth/token", config.api_base))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
        ]))
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Error::RateLimited {
            retry_after_secs: retry_after_secs(response.headers()),
        });
    }
    let body = response.text().await?;
    if !status.is_success() {
        return Err(Error::Api {
            status,
            // Bodies can be huge; 200 chars are plenty to diagnose.
            body: body.chars().take(200).collect(),
        });
    }
    let parsed = parse_token_body(&body, status)?;
    parsed.refresh_token.ok_or_else(|| Error::Api {
        status,
        body: "the token response carried no refresh token".to_owned(),
    })
}

/// Builds the one-time authorization URL. The scopes are read-only on
/// purpose: monitoring never needs control rights.
fn authorize_url(api_base: &str, client_id: &str, redirect_uri: &str) -> String {
    format!(
        "{api_base}/security/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope=IdentifyAppliance%20Monitor",
        form_escape(client_id),
        form_escape(redirect_uri),
    )
}

/// Returns the cached access token when it still clears the safety
/// margin. The stored instant already includes the margin.
fn valid_access(state: &TokenState) -> Option<String> {
    let cached = state.access.as_ref()?;
    if cached.expires_at > Instant::now() {
        Some(cached.token.clone())
    } else {
        None
    }
}

/// Parses a token endpoint success body. Home Connect sometimes reports a
/// failure inside a 200; surface it instead of a confusing field error.
fn parse_token_body(body: &str, status: reqwest::StatusCode) -> Result<TokenResponse> {
    let probe: ErrorProbe = serde_json::from_str(body)?;
    if let Some(error) = probe.error {
        return Err(Error::Api {
            status,
            body: format!("{}: {}", error.key, error.description),
        });
    }
    Ok(serde_json::from_str(body)?)
}

/// Builds one form-encoded request body from ordered field pairs.
fn form_body(fields: &[(&str, &str)]) -> String {
    let encoded = fields
        .iter()
        .map(|(name, value)| format!("{name}={}", form_escape(value)))
        .collect::<Vec<_>>();
    encoded.join("&")
}

/// Percent-encodes one form component. Everything outside the unreserved
/// set would otherwise be read as form structure, not data.
fn form_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                escaped.push(char::from(byte));
            }
            _ => {
                escaped.push('%');
                escaped.push(hex_digit(byte >> 4));
                escaped.push(hex_digit(byte & 0x0f));
            }
        }
    }
    escaped
}

/// One hex nibble as an uppercase digit, matching the documented form.
fn hex_digit(nibble: u8) -> char {
    // `from_digit` covers the whole nibble range; the fallback never runs.
    char::from_digit(u32::from(nibble), 16).map_or('0', |digit| digit.to_ascii_uppercase())
}

/// Reads the Retry-After header as seconds. Home Connect documents a
/// plain number; anything else falls back to one hour.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3600)
}

#[cfg(test)]
mod tests {
    // Axum handlers must be async even when they await nothing.
    #![allow(clippy::unused_async)]

    use std::sync::Arc;

    use axum::Router;
    use axum::extract::{RawQuery, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::{
        AuthListener, TokenManager, TokenResponse, auth_redirect, authorize_url, exchange_code,
        form_body, form_escape,
    };
    use crate::config::Config;
    use crate::error::Error;
    use crate::metrics::Metrics;
    use crate::stream::Budget;

    /// Records each token-endpoint hit as (content type, form body).
    #[derive(Default)]
    struct TokenMock {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    /// Serves a long-lived token, so one refresh covers the whole test.
    async fn token_ok(
        State(mock): State<Arc<TokenMock>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        record(&mock, &headers, body);
        // Locals keep the fixture values out of raw JSON strings.
        let access = "at-1";
        let refresh = "rt-once";
        let response = serde_json::json!({
            "access_token": access,
            "expires_in": 86400,
            "refresh_token": refresh,
            "token_type": "Bearer",
        })
        .to_string();
        ([(header::CONTENT_TYPE, "application/json")], response)
    }

    /// Serves an immediately stale token plus a rotated refresh token,
    /// so every `get` must refresh again.
    async fn token_rotating(
        State(mock): State<Arc<TokenMock>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        record(&mock, &headers, body);
        // One-based, so the first response already differs from the
        // configured refresh token.
        let n = mock.calls.lock().expect("mock state poisoned").len();
        let response = serde_json::json!({
            "access_token": format!("at-{n}"),
            "expires_in": 30,
            "refresh_token": format!("rt-{n}"),
        })
        .to_string();
        ([(header::CONTENT_TYPE, "application/json")], response)
    }

    /// Answers the documented rate limit with a Retry-After of 42 seconds.
    async fn token_rate_limited(
        State(mock): State<Arc<TokenMock>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        record(&mock, &headers, body);
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "42")],
            "slow down",
        )
    }

    /// Answers 500 with a body long enough to prove the 200-char cut.
    async fn token_server_error(
        State(mock): State<Arc<TokenMock>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        record(&mock, &headers, body);
        (StatusCode::INTERNAL_SERVER_ERROR, "x".repeat(300))
    }

    /// Answers 200 with the API's own error envelope.
    async fn token_error_envelope(
        State(mock): State<Arc<TokenMock>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        record(&mock, &headers, body);
        (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"key":"SDK.SecureToken.Error.InvalidGrant","description":"the grant expired"}}"#,
        )
    }

    /// Stores one hit's content type and raw form body.
    fn record(mock: &Arc<TokenMock>, headers: &HeaderMap, body: String) {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        mock.calls
            .lock()
            .expect("mock state poisoned")
            .push((content_type, body));
    }

    /// Starts `app` on an ephemeral local port and returns its base URL.
    /// The server task lives as long as the test's runtime.
    async fn spawn_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind to an ephemeral port");
        let addr = listener.local_addr().expect("read the local address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the mock server must keep running");
        });
        format!("http://{addr}")
    }

    /// A client that never consults proxy environment variables. The mock
    /// lives on localhost and a stray `http_proxy` would break the test.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("the test client must build")
    }

    /// A config whose API base points at the mock server.
    fn test_config(api_base: &str) -> Config {
        Config::build([
            ("HCE_CLIENT_ID", Some("test-id")),
            ("HCE_CLIENT_SECRET", Some("test-secret")),
            ("HCE_REFRESH_TOKEN", Some("initial-refresh")),
            ("HCE_API_BASE", Some(api_base)),
        ])
        .expect("the test config must parse")
    }

    /// A token manager over the mock-backed config and a fresh budget.
    fn manager(config: &Config, budget: i64) -> TokenManager {
        TokenManager::new(
            config,
            test_client(),
            Arc::new(Metrics::new()),
            Arc::new(Budget::new(budget)),
        )
    }

    #[test]
    fn the_documented_token_fixture_parses_and_ignores_unknown_fields() {
        // Locals keep the fixture values out of raw JSON strings.
        let access = "at-1";
        let refresh = "rt-once";
        let raw = serde_json::json!({
            "access_token": access,
            "expires_in": 86400,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "scope": "IdentifyAppliance",
        })
        .to_string();
        let parsed: TokenResponse = serde_json::from_str(&raw).expect("fixture must parse");
        assert_eq!(parsed.access_token, "at-1");
        assert_eq!(parsed.expires_in, 86400);
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt-once"));
    }

    #[test]
    fn a_token_response_without_a_refresh_token_is_tolerated() {
        let raw = r#"{"access_token": "***","expires_in":86400,"token_type":"Bearer"}"#;
        let parsed: TokenResponse = serde_json::from_str(raw).expect("fixture must parse");
        assert_eq!(parsed.refresh_token, None);
    }

    #[test]
    fn form_bodies_follow_the_documented_order_and_encoding() {
        assert_eq!(
            form_body(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", "initial-refresh"),
                ("client_id", "test-id"),
                ("client_secret", "test-secret"),
            ]),
            "grant_type=refresh_token&refresh_token=initial-refresh&client_id=test-id&client_secret=test-secret"
        );
        // Anything that could read as form structure must be escaped.
        assert_eq!(form_escape("a b&c/d"), "a%20b%26c%2Fd");
        assert_eq!(form_escape("keukén"), "keuk%C3%A9n");
    }

    #[test]
    fn the_authorize_url_is_built_with_read_only_scopes() {
        assert_eq!(
            authorize_url(
                "https://api.home-connect.com",
                "test id",
                "http://127.0.0.1:8888/auth",
            ),
            "https://api.home-connect.com/security/oauth/authorize?response_type=code&client_id=test%20id&redirect_uri=http%3A%2F%2F127.0.0.1%3A8888%2Fauth&scope=IdentifyAppliance%20Monitor"
        );
    }

    #[tokio::test]
    async fn get_refreshes_once_then_serves_from_the_cache() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_ok))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let manager = manager(&test_config(&base), 10);

        assert_eq!(manager.get().await.expect("first get"), "at-1");
        assert_eq!(manager.get().await.expect("second get"), "at-1");

        let calls = mock.calls.lock().expect("mock state poisoned").clone();
        assert_eq!(calls.len(), 1);
        let (content_type, body) = calls.first().expect("exactly one call");
        assert_eq!(content_type, "application/x-www-form-urlencoded");
        assert_eq!(
            body,
            "grant_type=refresh_token&refresh_token=initial-refresh&client_id=test-id&client_secret=test-secret"
        );
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_stored_and_sent_next_time() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_rotating))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let manager = manager(&test_config(&base), 10);

        // The served token lives inside the safety margin, so every get
        // refreshes.
        assert_eq!(manager.get().await.expect("first get"), "at-1");
        assert_eq!(manager.get().await.expect("second get"), "at-2");

        let bodies: Vec<String> = mock
            .calls
            .lock()
            .expect("mock state poisoned")
            .iter()
            .map(|(_, body)| body.clone())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies.first().map(String::as_str),
            Some(
                "grant_type=refresh_token&refresh_token=initial-refresh&client_id=test-id&client_secret=test-secret"
            )
        );
        // The second call proves the rotated token from response one was
        // stored.
        assert_eq!(
            bodies.get(1).map(String::as_str),
            Some(
                "grant_type=refresh_token&refresh_token=rt-1&client_id=test-id&client_secret=test-secret"
            )
        );
    }

    #[tokio::test]
    async fn a_spent_budget_still_serves_a_cached_token() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_ok))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        // One unit: the first refresh consumes the whole budget.
        let manager = manager(&test_config(&base), 1);

        assert_eq!(manager.get().await.expect("first get"), "at-1");
        assert_eq!(manager.get().await.expect("second get"), "at-1");
        assert_eq!(mock.calls.lock().expect("mock state poisoned").len(), 1);
    }

    #[tokio::test]
    async fn a_spent_budget_without_a_cached_token_fails_loudly() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_ok))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        // A budget of zero: the first refresh is already impossible.
        let manager = manager(&test_config(&base), 0);

        let err = manager.get().await.expect_err("no budget, no cache");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        && body == "local daily budget spent"
            ),
            "expected the local budget error, got {err:?}"
        );
        // The token endpoint was never contacted.
        assert!(mock.calls.lock().expect("mock state poisoned").is_empty());
    }

    #[tokio::test]
    async fn a_429_maps_to_the_retry_after_header() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_rate_limited))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let manager = manager(&test_config(&base), 10);

        let err = manager.get().await.expect_err("the mock answers 429");
        assert!(
            matches!(
                &err,
                Error::RateLimited {
                    retry_after_secs: 42
                }
            ),
            "expected RateLimited(42), got {err:?}"
        );
        // The call went out before the server said no.
        assert_eq!(mock.calls.lock().expect("mock state poisoned").len(), 1);
    }

    #[tokio::test]
    async fn a_failed_refresh_reports_the_first_200_body_chars() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_server_error))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let manager = manager(&test_config(&base), 10);

        let err = manager.get().await.expect_err("the mock answers 500");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
                        && *body == "x".repeat(200)
            ),
            "expected a truncated Api error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_error_envelope_with_a_200_maps_to_an_api_error() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_error_envelope))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let manager = manager(&test_config(&base), 10);

        let err = manager
            .get()
            .await
            .expect_err("the mock answers an error envelope");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == reqwest::StatusCode::OK
                        && body == "SDK.SecureToken.Error.InvalidGrant: the grant expired"
            ),
            "expected the envelope error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn the_code_exchange_posts_the_documented_form_and_returns_the_refresh_token() {
        let mock = Arc::new(TokenMock::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_ok))
            .with_state(mock.clone());
        let base = spawn_mock(app).await;
        let config = test_config(&base);

        let refresh = exchange_code(
            &test_client(),
            &config,
            "test-code",
            "http://127.0.0.1:1/auth",
        )
        .await
        .expect("the exchange must succeed");
        assert_eq!(refresh, "rt-once");

        let calls = mock.calls.lock().expect("mock state poisoned").clone();
        assert_eq!(
            calls.first().map(|(_, body)| body.as_str()),
            Some(
                "grant_type=authorization_code&code=test-code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fauth&client_id=test-id&client_secret=test-secret"
            )
        );
    }

    #[tokio::test]
    async fn the_redirect_handler_delivers_the_code_and_stops_the_flow() {
        let (code_tx, mut code_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let listener = Arc::new(AuthListener {
            code: code_tx,
            done: done_tx,
        });

        // The value arrives percent-encoded, as a browser would send it.
        let query = RawQuery(Some(String::from("state=x&code=auth%2Dcode")));
        let response = auth_redirect(State(listener.clone()), query)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(code_rx.recv().await, Some(String::from("auth-code")));
        assert!(*done_rx.borrow());

        // A redirect without a code must not stop the flow.
        let response = auth_redirect(State(listener), RawQuery(None))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
