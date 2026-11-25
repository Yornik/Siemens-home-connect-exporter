//! One long-lived task per appliance: seed a status snapshot, then follow
//! the Server-Sent Events stream until the process ends.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::api::{self, Appliance, StatusItem};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::events::{self, EventItem, MappedUpdate, StreamEvent};
use crate::metrics::Metrics;
use crate::oauth::TokenManager;
use crate::sse::SseParser;

/// Shared daily-request budget across all appliance tasks.
/// The SSE stream itself is free; every new connection, snapshot or refresh
/// takes one unit. Home Connect counts 1000 requests per client and user
/// per day, so the default keeps a reserve.
pub struct Budget {
    remaining: AtomicI64,
}

impl Budget {
    /// Creates a budget of `limit` requests per day.
    #[must_use]
    pub fn new(limit: i64) -> Self {
        Self {
            remaining: AtomicI64::new(limit),
        }
    }

    /// Takes one request from the budget.
    /// Returns false when the budget is spent; the caller then skips the
    /// call and keeps its existing stream open.
    pub fn take(&self) -> bool {
        self.remaining.fetch_sub(1, Ordering::Relaxed) > 0
    }
}

/// Runs the follow loop for one appliance. This task never ends.
/// Every failure waits for a backoff, then reconnects; the backoff doubles
/// up to 5 minutes and resets after a stream that was healthy.
pub async fn run(
    client: reqwest::Client,
    config: Config,
    tokens: Arc<TokenManager>,
    metrics: Arc<Metrics>,
    budget: Arc<Budget>,
    appliance: Appliance,
) {
    let label = display_name(&appliance);
    let mut backoff = Duration::from_secs(5);
    loop {
        match once(
            &client, &config, &tokens, &metrics, &budget, &appliance, &label,
        )
        .await
        {
            Ok(()) => {
                metrics.set_connected(&label, false);
                info!(appliance = %label, "stream closed; reconnecting");
                backoff = Duration::from_secs(5);
            }
            Err(Error::RateLimited { retry_after_secs }) => {
                warn!(appliance = %label, retry_after_secs, "rate limited");
                let wait = Duration::from_secs(retry_after_secs.min(3600));
                sleep(wait).await;
                backoff = Duration::from_secs(5);
            }
            Err(err) => {
                warn!(appliance = %label, error = %err, "stream failed; reconnecting");
                sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(300));
            }
        }
    }
}

/// One pass: snapshot, then stream until the server closes the channel.
async fn once(
    client: &reqwest::Client,
    config: &Config,
    tokens: &Arc<TokenManager>,
    metrics: &Arc<Metrics>,
    budget: &Arc<Budget>,
    appliance: &Appliance,
    label: &str,
) -> Result<()> {
    let token = tokens.get().await?;

    // Snapshot: fills every gauge before the first event arrives.
    if budget.take() {
        metrics.count_api_request("status");
        let status = api::get_status(client, &config.api_base, &token, &appliance.ha_id).await;
        match status {
            Ok(items) => apply_items(metrics, label, &snapshot_items(&items)),
            Err(err) => {
                metrics.count_api_error("status");
                debug!(appliance = %label, error = %err, "snapshot failed; continuing to the stream");
            }
        }
    } else {
        warn!(appliance = %label, "daily budget spent; skipping snapshot");
    }

    if !budget.take() {
        // No fresh stream without budget: retry later through the loop.
        return Err(Error::Api {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "local daily budget spent".to_owned(),
        });
    }
    metrics.count_api_request("events");
    let url = format!(
        "{}/api/homeappliances/{}/events",
        config.api_base, appliance.ha_id
    );
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(3600);
        return Err(Error::RateLimited {
            retry_after_secs: retry_after,
        });
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Api {
            status,
            body: body.chars().take(200).collect(),
        });
    }
    info!(appliance = %label, "stream open");

    let stream = response.bytes_stream();
    tokio::pin!(stream);
    let mut parser = SseParser::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for message in parser.push(&chunk) {
            handle_message(metrics, label, &message);
        }
    }
    Ok(())
}

/// Applies one parsed SSE message to the metrics.
fn handle_message(metrics: &Metrics, label: &str, message: &crate::sse::SseMessage) {
    match events::classify(message) {
        Some(StreamEvent::Connected) => metrics.set_connected(label, true),
        Some(StreamEvent::Disconnected) => metrics.set_connected(label, false),
        Some(StreamEvent::KeepAlive) => {}
        Some(StreamEvent::Paired | StreamEvent::Depaired) => {
            debug!(appliance = %label, "pairing change; a restart picks it up");
        }
        Some(StreamEvent::Items(items)) => apply_items(metrics, label, &items),
        None => debug!(appliance = %label, "unclassifiable message skipped"),
    }
}

/// Applies every mapped item to the metrics.
fn apply_items(metrics: &Metrics, label: &str, items: &[EventItem]) {
    for item in items {
        match events::map_item(item) {
            Some(MappedUpdate::OperationState(state)) => {
                metrics.set_operation_state(label, &state);
            }
            Some(MappedUpdate::DoorOpen(open)) => metrics.set_door_open(label, open),
            Some(MappedUpdate::ProgramProgress(percent)) => {
                metrics.set_program_progress(label, percent);
            }
            Some(MappedUpdate::TimeRemainingSecs(secs)) => {
                metrics.set_time_remaining(label, secs);
            }
            Some(MappedUpdate::TimeElapsedSecs(secs)) => {
                metrics.set_time_elapsed(label, secs);
            }
            Some(MappedUpdate::ActiveProgram(program)) => {
                metrics.set_active_program(label, &program);
            }
            Some(MappedUpdate::ProgramEvent(event)) => {
                metrics.count_program_event(label, &event);
            }
            None => {}
        }
    }
}

/// Converts a status snapshot into event-shaped items so one code path
/// handles both.
fn snapshot_items(status: &[StatusItem]) -> Vec<EventItem> {
    status
        .iter()
        .map(|item| EventItem {
            key: item.key.clone(),
            value: item.value.clone(),
        })
        .collect()
}

/// The label value for one appliance: its name, or the haId when no name
/// is set.
#[must_use]
pub fn display_name(appliance: &Appliance) -> String {
    if appliance.name.is_empty() {
        appliance.ha_id.clone()
    } else {
        appliance.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_items, display_name, snapshot_items};
    use crate::api::Appliance;
    use crate::events::EventItem;

    fn appliance(name: &str) -> Appliance {
        Appliance {
            ha_id: "SIEMENS-SN678D10TG-AB1234567890".to_owned(),
            name: name.to_owned(),
            brand: "SIEMENS".to_owned(),
            appliance_type: "Dishwasher".to_owned(),
            connected: false,
        }
    }

    #[test]
    fn an_empty_name_falls_back_to_the_ha_id() {
        assert_eq!(
            display_name(&appliance("")),
            "SIEMENS-SN678D10TG-AB1234567890"
        );
        assert_eq!(display_name(&appliance("Keuken")), "Keuken");
    }

    #[test]
    fn snapshot_items_keep_key_and_value() {
        let status = vec![crate::api::StatusItem {
            key: "BSH.Common.Status.OperationState".to_owned(),
            value: serde_json::json!("BSH.Common.EnumType.OperationState.Run"),
        }];
        let items = snapshot_items(&status);
        assert_eq!(
            items,
            vec![EventItem {
                key: "BSH.Common.Status.OperationState".to_owned(),
                value: serde_json::json!("BSH.Common.EnumType.OperationState.Run"),
            }]
        );
    }

    #[test]
    fn the_budget_releases_and_then_blocks() {
        let budget = super::Budget::new(2);
        assert!(budget.take());
        assert!(budget.take());
        assert!(!budget.take());
    }

    #[test]
    fn stream_events_update_the_real_metrics_and_zero_the_previous_state() {
        use crate::events::{EventItem, StreamEvent};
        use crate::metrics::Metrics;
        use crate::sse::SseMessage;

        fn items(payload: &str) -> Option<Vec<EventItem>> {
            let message = SseMessage {
                event: Some("STATUS".to_owned()),
                data: Some(payload.to_owned()),
                id: None,
            };
            match crate::events::classify(&message) {
                Some(StreamEvent::Items(items)) => Some(items),
                _ => None,
            }
        }

        let metrics = Metrics::new();
        let running = items(
            r#"{"haId":"x","items":[{"key":"BSH.Common.Status.OperationState","value":"BSH.Common.EnumType.OperationState.Run"},{"key":"BSH.Common.Option.ProgramProgress","value":91}]}"#,
        )
        .expect("the fixture must classify as items");
        apply_items(&metrics, "Keuken", &running);
        let finished = items(
            r#"{"haId":"x","items":[{"key":"BSH.Common.Status.OperationState","value":"BSH.Common.EnumType.OperationState.Finished"}]}"#,
        )
        .expect("the fixture must classify as items");
        apply_items(&metrics, "Keuken", &finished);
        let text = metrics.encode().expect("encode must succeed");
        assert!(
            text.contains("homeconnect_operation_state{appliance=\"Keuken\",state=\"Run\"} 0"),
            "the previous state must read zero: {text}"
        );
        assert!(
            text.contains("homeconnect_operation_state{appliance=\"Keuken\",state=\"Finished\"} 1"),
            "the new state must read one: {text}"
        );
        assert!(
            text.contains("homeconnect_program_progress_percent{appliance=\"Keuken\"} 91"),
            "the progress gauge must read the event value: {text}"
        );
    }
}
