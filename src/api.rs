//! Home Connect REST calls: the appliance list and a status snapshot.
//!
//! Every response hides its payload inside a `data` envelope. The private
//! serde types here unwrap it and ignore fields this exporter does not
//! use.

use crate::error::{Error, Result};

/// The media type every REST call must accept. Home Connect gates its
/// response shape on it.
const ACCEPT: &str = "application/vnd.bsh.sdk.v1+json";

/// One Home Connect appliance as the `/api/homeappliances` endpoint reports it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Appliance {
    /// Home Connect identifier, e.g. `SIEMENS-SN678D10TG-AB1234567890`.
    #[serde(rename = "haId")]
    pub ha_id: String,
    /// Human-assigned name. May be empty; the stream loop falls back to haId.
    #[serde(default)]
    pub name: String,
    /// Brand, e.g. `SIEMENS`.
    #[serde(default)]
    pub brand: String,
    /// Appliance class, e.g. `Dishwasher`, `CoffeeMaker`, `Hob`.
    #[serde(rename = "type", default)]
    pub appliance_type: String,
    /// Current cloud-connection state; live updates come from the stream.
    #[serde(default)]
    pub connected: bool,
}

/// One key-value status entry. The value shape depends on the key: strings
/// for enum states, numbers for counters, booleans for flags.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StatusItem {
    /// Fully-qualified Home Connect key, e.g. `BSH.Common.Status.OperationState`.
    pub key: String,
    /// The value as the API reports it.
    #[serde(default)]
    pub value: serde_json::Value,
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

/// The `{"data": ..., "error": ...}` envelope around every REST payload.
#[derive(serde::Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    error: Option<ApiErrorBody>,
}

/// The appliance-list payload inside the envelope.
#[derive(Debug, Default, serde::Deserialize)]
struct ApplianceList {
    #[serde(default)]
    homeappliances: Vec<Appliance>,
}

/// The status-snapshot payload inside the envelope.
#[derive(Debug, Default, serde::Deserialize)]
struct StatusPayload {
    #[serde(default)]
    status: Vec<StatusItem>,
}

/// Lists every appliance registered to the account.
///
/// # Errors
/// Returns an error when the call fails or the payload does not parse.
pub async fn list_appliances(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
) -> Result<Vec<Appliance>> {
    let url = format!("{api_base}/api/homeappliances");
    let (status, body) = get_body(client, &url, token).await?;
    let payload: ApplianceList = parse_body(&body, status)?;
    Ok(payload.homeappliances)
}

/// Reads the full status snapshot of one appliance.
///
/// # Errors
/// Returns an error when the call fails or the payload does not parse.
pub async fn get_status(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    ha_id: &str,
) -> Result<Vec<StatusItem>> {
    let url = format!("{api_base}/api/homeappliances/{ha_id}/status");
    let (status, body) = get_body(client, &url, token).await?;
    let payload: StatusPayload = parse_body(&body, status)?;
    Ok(payload.status)
}

/// Performs one authenticated GET and returns the status and body of a
/// successful response. 429 and other failures map onto the crate's
/// error enum.
async fn get_body(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<(reqwest::StatusCode, String)> {
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, ACCEPT)
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Home Connect sends the wait time in seconds.
        return Err(Error::RateLimited {
            retry_after_secs: retry_after_secs(response.headers()),
        });
    }
    let body = response.text().await?;
    if !status.is_success() {
        // Bodies can be huge; the first 200 chars are plenty to diagnose.
        return Err(Error::Api {
            status,
            body: body.chars().take(200).collect(),
        });
    }
    Ok((status, body))
}

/// Turns one successful REST body into its `data` payload.
///
/// Home Connect reports some failures inside a 200; that error envelope
/// wins over any payload. Empty bodies and missing payloads parse as the
/// empty default, which matches an empty data envelope.
fn parse_body<T>(body: &str, status: reqwest::StatusCode) -> Result<T>
where
    T: Default + serde::de::DeserializeOwned,
{
    if body.trim().is_empty() {
        return Ok(T::default());
    }
    let envelope: Envelope<T> = serde_json::from_str(body)?;
    if let Some(error) = envelope.error {
        return Err(Error::Api {
            status,
            body: format!("{}: {}", error.key, error.description),
        });
    }
    Ok(envelope.data.unwrap_or_default())
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
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    use super::{ApplianceList, Envelope, StatusPayload, get_status, list_appliances, parse_body};
    use crate::config::Config;
    use crate::error::Error;
    use crate::metrics::Metrics;
    use crate::oauth::TokenManager;
    use crate::stream::Budget;

    #[test]
    fn the_appliance_fixture_parses_and_ignores_unknown_fields() {
        let raw = r#"{"data":{"homeappliances":[{"haId":"SIEMENS-SN678D10TG-AB1234567890","name":"Keuken","brand":"SIEMENS","type":"Dishwasher","enumber":"SN678D10TG","connected":true}]}}"#;
        let parsed: Envelope<ApplianceList> =
            serde_json::from_str(raw).expect("fixture must parse");
        let appliances = parsed.data.expect("data present").homeappliances;
        assert_eq!(appliances.len(), 1);
        let appliance = appliances.first().expect("one appliance");
        assert_eq!(appliance.ha_id, "SIEMENS-SN678D10TG-AB1234567890");
        assert_eq!(appliance.name, "Keuken");
        assert_eq!(appliance.brand, "SIEMENS");
        assert_eq!(appliance.appliance_type, "Dishwasher");
        assert!(appliance.connected);
    }

    #[test]
    fn an_appliance_without_optional_fields_defaults_them() {
        let raw = r#"{"data":{"homeappliances":[{"haId":"SIEMENS-SN678D10TG-AB1234567890"}]}}"#;
        let parsed: Envelope<ApplianceList> =
            serde_json::from_str(raw).expect("fixture must parse");
        let payload = parsed.data.expect("data present");
        let appliance = payload.homeappliances.first().expect("one appliance");
        assert_eq!(appliance.name, "");
        assert_eq!(appliance.brand, "");
        assert_eq!(appliance.appliance_type, "");
        assert!(!appliance.connected);
    }

    #[test]
    fn the_status_fixture_parses_values_of_every_shape() {
        let raw = r#"{"data":{"status":[{"key":"BSH.Common.Status.OperationState","value":"BSH.Common.EnumType.OperationState.Run"},{"key":"BSH.Common.Status.DoorState","value":"BSH.Common.EnumType.DoorState.Closed"},{"key":"BSH.Common.Status.ProgramProgress","value":42,"unit":"%"}]}}"#;
        let parsed: Envelope<StatusPayload> =
            serde_json::from_str(raw).expect("fixture must parse");
        let status = parsed.data.expect("data present").status;
        assert_eq!(status.len(), 3);
        assert_eq!(
            status.first().map(|item| item.value.clone()),
            Some(serde_json::json!("BSH.Common.EnumType.OperationState.Run"))
        );
        assert_eq!(
            status.get(1).map(|item| item.key.clone()),
            Some("BSH.Common.Status.DoorState".to_owned())
        );
        assert_eq!(
            status.get(2).map(|item| item.value.clone()),
            Some(serde_json::json!(42))
        );
    }

    #[test]
    fn parse_body_treats_empty_bodies_as_empty_envelopes() {
        let payload: ApplianceList =
            parse_body("  ", StatusCode::OK).expect("an empty body must parse");
        assert!(payload.homeappliances.is_empty());
    }

    #[test]
    fn parse_body_treats_absent_payloads_as_empty() {
        let raw = r#"{"no":"data here"}"#;
        let payload: StatusPayload =
            parse_body(raw, StatusCode::OK).expect("a data-less body must parse");
        assert!(payload.status.is_empty());
    }

    #[test]
    fn parse_body_maps_a_200_error_envelope_to_an_api_error() {
        let raw = r#"{"error":{"key":"SDK.HomeAppliance.Error.NotAuthorized","description":"unknown appliance"}}"#;
        let err = parse_body::<ApplianceList>(raw, StatusCode::OK)
            .expect_err("the envelope reports an error");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == StatusCode::OK
                        && body == "SDK.HomeAppliance.Error.NotAuthorized: unknown appliance"
            ),
            "expected the envelope error, got {err:?}"
        );
    }

    /// Records what the real client code sent, so the tests can assert on
    /// headers and paths, not just on return values.
    #[derive(Default)]
    struct Recording {
        /// The raw form body of each token request.
        token_bodies: std::sync::Mutex<Vec<String>>,
        /// The (authorization, accept) pairs the appliance list was called with.
        appliance_headers: std::sync::Mutex<Vec<(String, String)>>,
        /// The (authorization, accept) pairs the status snapshot was called with.
        status_headers: std::sync::Mutex<Vec<(String, String)>>,
        /// The haId path segments the status route served.
        status_ha_ids: std::sync::Mutex<Vec<String>>,
    }

    /// Reads one header as an owned string, empty when absent.
    fn header_value(headers: &HeaderMap, name: header::HeaderName) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// Serves the documented token payload with a rotated refresh token.
    async fn token_route(
        State(recording): State<Arc<Recording>>,
        body: String,
    ) -> impl IntoResponse {
        recording
            .token_bodies
            .lock()
            .expect("mock state poisoned")
            .push(body);
        // Locals keep the fixture values out of raw JSON strings.
        let access = "access-1";
        let refresh = "rt-once";
        let response = serde_json::json!({
            "access_token": access,
            "expires_in": 86400,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "scope": "IdentifyAppliance",
        })
        .to_string();
        ([(header::CONTENT_TYPE, "application/json")], response)
    }

    /// Serves the documented appliance list with unknown extra fields.
    async fn appliances_route(
        State(recording): State<Arc<Recording>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let pair = (
            header_value(&headers, header::AUTHORIZATION),
            header_value(&headers, header::ACCEPT),
        );
        recording
            .appliance_headers
            .lock()
            .expect("mock state poisoned")
            .push(pair);
        (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"data":{"homeappliances":[{"haId":"SIEMENS-SN678D10TG-AB1234567890","name":"Keuken","brand":"SIEMENS","type":"Dishwasher","enumber":"SN678D10TG","connected":true}]}}"#,
        )
    }

    /// Serves the documented status snapshot for whatever haId arrives.
    async fn status_route(
        State(recording): State<Arc<Recording>>,
        Path(ha_id): Path<String>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        recording
            .status_ha_ids
            .lock()
            .expect("mock state poisoned")
            .push(ha_id);
        let pair = (
            header_value(&headers, header::AUTHORIZATION),
            header_value(&headers, header::ACCEPT),
        );
        recording
            .status_headers
            .lock()
            .expect("mock state poisoned")
            .push(pair);
        (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"data":{"status":[{"key":"BSH.Common.Status.OperationState","value":"BSH.Common.EnumType.OperationState.Run"},{"key":"BSH.Common.Status.DoorState","value":"BSH.Common.EnumType.DoorState.Closed"},{"key":"BSH.Common.Status.ProgramProgress","value":42,"unit":"%"}]}}"#,
        )
    }

    /// Answers 429 with the documented Retry-After in seconds.
    async fn rate_limited() -> impl IntoResponse {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "42")],
            "too many requests",
        )
    }

    /// Answers 429 without a Retry-After header.
    async fn rate_limited_without_header() -> impl IntoResponse {
        (StatusCode::TOO_MANY_REQUESTS, String::new())
    }

    /// Answers 500 with a body long enough to prove the 200-char cut.
    async fn server_error() -> impl IntoResponse {
        (StatusCode::INTERNAL_SERVER_ERROR, "x".repeat(300))
    }

    /// Answers 200 with no content at all.
    async fn empty_body() -> impl IntoResponse {
        (StatusCode::OK, String::new())
    }

    /// Answers 200 with the API's own error envelope.
    async fn error_envelope() -> impl IntoResponse {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"key":"SDK.HomeAppliance.Error.NotAuthorized","description":"unknown appliance"}}"#,
        )
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

    #[tokio::test]
    async fn token_appliances_and_status_flow_against_a_mock_server() {
        let recording = Arc::new(Recording::default());
        let app = Router::new()
            .route("/security/oauth/token", post(token_route))
            .route("/api/homeappliances", get(appliances_route))
            .route("/api/homeappliances/{ha_id}/status", get(status_route))
            .with_state(recording.clone());
        let base = spawn_mock(app).await;

        let config = Config::build([
            ("HCE_CLIENT_ID", Some("test-id")),
            ("HCE_CLIENT_SECRET", Some("test-secret")),
            ("HCE_REFRESH_TOKEN", Some("initial-refresh")),
            ("HCE_API_BASE", Some(base.as_str())),
        ])
        .expect("the test config must parse");
        let client = test_client();
        let metrics = Arc::new(Metrics::new());
        let budget = Arc::new(Budget::new(100));
        let tokens = Arc::new(TokenManager::new(&config, client.clone(), metrics, budget));

        let token = tokens.get().await.expect("the token refresh");
        assert_eq!(token, "access-1");

        let appliances = list_appliances(&client, &config.api_base, &token)
            .await
            .expect("the appliance list");
        assert_eq!(appliances.len(), 1);
        let appliance = appliances.first().expect("one appliance");
        assert_eq!(appliance.ha_id, "SIEMENS-SN678D10TG-AB1234567890");
        assert_eq!(appliance.name, "Keuken");
        assert_eq!(appliance.brand, "SIEMENS");
        assert_eq!(appliance.appliance_type, "Dishwasher");
        assert!(appliance.connected);

        let status = get_status(&client, &config.api_base, &token, &appliance.ha_id)
            .await
            .expect("the status snapshot");
        assert_eq!(status.len(), 3);
        assert_eq!(
            status.first().map(|item| item.key.clone()),
            Some("BSH.Common.Status.OperationState".to_owned())
        );
        assert_eq!(
            status.first().map(|item| item.value.clone()),
            Some(serde_json::json!("BSH.Common.EnumType.OperationState.Run"))
        );

        // The token endpoint saw one refresh with the documented form.
        let bodies = recording
            .token_bodies
            .lock()
            .expect("mock state poisoned")
            .clone();
        assert_eq!(
            bodies.first().map(String::as_str),
            Some(
                "grant_type=refresh_token&refresh_token=initial-refresh&client_id=test-id&client_secret=test-secret"
            )
        );

        // Both REST calls carried the token and the SDK media type.
        let expected_headers = (
            "Bearer access-1".to_owned(),
            "application/vnd.bsh.sdk.v1+json".to_owned(),
        );
        assert_eq!(
            recording
                .appliance_headers
                .lock()
                .expect("mock state poisoned")
                .first()
                .cloned(),
            Some(expected_headers.clone())
        );
        assert_eq!(
            recording
                .status_headers
                .lock()
                .expect("mock state poisoned")
                .first()
                .cloned(),
            Some(expected_headers)
        );
        assert_eq!(
            recording
                .status_ha_ids
                .lock()
                .expect("mock state poisoned")
                .first()
                .cloned(),
            Some("SIEMENS-SN678D10TG-AB1234567890".to_owned())
        );

        // A later get is served from the cache without a new refresh.
        assert_eq!(tokens.get().await.expect("the cached token"), "access-1");
        assert_eq!(
            recording
                .token_bodies
                .lock()
                .expect("mock state poisoned")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rate_limit_answers_map_to_retry_after_seconds() {
        let app = Router::new()
            .route("/api/homeappliances", get(rate_limited))
            .route("/api/homeappliances/{ha_id}/status", get(rate_limited));
        let base = spawn_mock(app).await;
        let client = test_client();

        let err = list_appliances(&client, &base, "token")
            .await
            .expect_err("the mock answers 429");
        assert!(
            matches!(
                &err,
                Error::RateLimited {
                    retry_after_secs: 42
                }
            ),
            "expected RateLimited(42), got {err:?}"
        );

        let err = get_status(&client, &base, "token", "SIEMENS-SN678D10TG-AB1234567890")
            .await
            .expect_err("the mock answers 429");
        assert!(
            matches!(
                &err,
                Error::RateLimited {
                    retry_after_secs: 42
                }
            ),
            "expected RateLimited(42), got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_429_without_retry_after_falls_back_to_one_hour() {
        let app = Router::new().route("/api/homeappliances", get(rate_limited_without_header));
        let base = spawn_mock(app).await;
        let client = test_client();

        let err = list_appliances(&client, &base, "token")
            .await
            .expect_err("the mock answers 429");
        assert!(
            matches!(
                &err,
                Error::RateLimited {
                    retry_after_secs: 3600
                }
            ),
            "expected RateLimited(3600), got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_call_reports_the_first_200_body_chars() {
        let app = Router::new().route("/api/homeappliances", get(server_error));
        let base = spawn_mock(app).await;
        let client = test_client();

        let err = list_appliances(&client, &base, "token")
            .await
            .expect_err("the mock answers 500");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == StatusCode::INTERNAL_SERVER_ERROR
                        && *body == "x".repeat(200)
            ),
            "expected a truncated Api error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_body_parses_as_an_empty_envelope() {
        let app = Router::new()
            .route("/api/homeappliances", get(empty_body))
            .route("/api/homeappliances/{ha_id}/status", get(empty_body));
        let base = spawn_mock(app).await;
        let client = test_client();

        let appliances = list_appliances(&client, &base, "token")
            .await
            .expect("an empty list must parse");
        assert!(appliances.is_empty());
        let status = get_status(&client, &base, "token", "SIEMENS-SN678D10TG-AB1234567890")
            .await
            .expect("an empty status must parse");
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn an_error_envelope_with_a_200_maps_to_an_api_error() {
        let app = Router::new().route("/api/homeappliances", get(error_envelope));
        let base = spawn_mock(app).await;
        let client = test_client();

        let err = list_appliances(&client, &base, "token")
            .await
            .expect_err("the mock answers an error envelope");
        assert!(
            matches!(
                &err,
                Error::Api { status, body }
                    if *status == StatusCode::OK
                        && body == "SDK.HomeAppliance.Error.NotAuthorized: unknown appliance"
            ),
            "expected the envelope error, got {err:?}"
        );
    }
}
