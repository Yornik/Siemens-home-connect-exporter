//! Home Connect event classification and item mapping.
//!
//! The follow loop feeds every parsed SSE message through `classify`; only
//! messages with a known stream meaning survive. `map_item` then narrows
//! items down to the few keys this exporter exposes, so unknown traffic
//! never reaches the metrics.

use serde::Deserialize;
use serde_json::Value;

use crate::sse::SseMessage;

/// The stream-level meaning of one SSE message.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The appliance reached the Home Connect cloud.
    Connected,
    /// The appliance lost the cloud connection.
    Disconnected,
    /// Heartbeat; carries no data.
    KeepAlive,
    /// A new appliance paired to the account.
    Paired,
    /// An appliance was removed from the account.
    Depaired,
    /// A STATUS, EVENT or NOTIFY payload with key-value items.
    Items(Vec<EventItem>),
}

/// One key-value item from a STATUS, EVENT or NOTIFY payload.
#[derive(Debug, Clone, PartialEq)]
pub struct EventItem {
    /// Fully-qualified Home Connect key.
    pub key: String,
    /// The value as the API reports it.
    pub value: serde_json::Value,
}

/// One metric-level update derived from an item.
#[derive(Debug, Clone, PartialEq)]
pub enum MappedUpdate {
    /// Operation state; the short enum label, e.g. `Run`, `Finished`.
    OperationState(String),
    /// Door open flag.
    DoorOpen(bool),
    /// Program progress in percent.
    ProgramProgress(f64),
    /// Remaining program time in seconds.
    TimeRemainingSecs(f64),
    /// Elapsed program time in seconds.
    TimeElapsedSecs(f64),
    /// Active program; the short label, e.g. `Eco50`.
    ActiveProgram(String),
    /// A one-shot program event, e.g. `ProgramFinished`.
    ProgramEvent(String),
}

/// Classifies one SSE message.
/// Returns `None` when the message carries no classifiable payload, for
/// example an unknown event name or unparseable data.
#[must_use]
pub fn classify(message: &SseMessage) -> Option<StreamEvent> {
    match message.event.as_deref() {
        Some("STATUS" | "EVENT" | "NOTIFY") => message.data.as_deref().and_then(items_from),
        Some("CONNECTED") => Some(StreamEvent::Connected),
        Some("DISCONNECTED") => Some(StreamEvent::Disconnected),
        Some("PAIRED") => Some(StreamEvent::Paired),
        Some("DEPAIRED") => Some(StreamEvent::Depaired),
        // The stream heartbeats as `KEEP-ALIVE`, or with an empty payload.
        _ if heartbeat(message) => Some(StreamEvent::KeepAlive),
        _ => None,
    }
}

/// Tells heartbeats from traffic: either the KEEP-ALIVE name arrives, or a
/// message with nothing worth parsing.
fn heartbeat(message: &SseMessage) -> bool {
    if message.event.as_deref() == Some("KEEP-ALIVE") {
        return true;
    }
    message.data.as_deref().is_none_or(str::is_empty)
}

/// Builds the item list of a STATUS, EVENT or NOTIFY payload.
/// Returns `None` for invalid JSON: half a message would desync the
/// metrics.
fn items_from(data: &str) -> Option<StreamEvent> {
    let payload: Payload = serde_json::from_str(data).ok()?;
    let items = payload
        .items
        .unwrap_or_default()
        .into_iter()
        // One malformed item must not discard its healthy siblings.
        .filter_map(|raw| serde_json::from_value::<RawItem>(raw).ok())
        .map(|item| EventItem {
            key: item.key,
            value: item.value,
        })
        .collect();
    Some(StreamEvent::Items(items))
}

/// The JSON envelope of a STATUS, EVENT or NOTIFY message.
#[derive(Deserialize)]
struct Payload {
    /// `items` is absent when nothing changed; absent and empty both mean
    /// "no items", not "broken message".
    items: Option<Vec<Value>>,
}

/// One wire item. Fields beyond `key` and `value` never become metrics.
#[derive(Deserialize)]
struct RawItem {
    key: String,
    /// A missing value deserializes to null; mapping then yields `None`.
    #[serde(default)]
    value: Value,
}

/// Maps one item onto a metric update.
/// Returns `None` for keys this exporter does not expose, for example
/// remote-control flags.
#[must_use]
pub fn map_item(item: &EventItem) -> Option<MappedUpdate> {
    match item.key.as_str() {
        "BSH.Common.Status.OperationState" => {
            short_enum(&item.value).map(MappedUpdate::OperationState)
        }
        "BSH.Common.Status.DoorState" => door_state(&item.value),
        "BSH.Common.Option.ProgramProgress" => {
            number(&item.value).map(MappedUpdate::ProgramProgress)
        }
        "BSH.Common.Option.RemainingProgramTime" => {
            number(&item.value).map(MappedUpdate::TimeRemainingSecs)
        }
        "BSH.Common.Option.ElapsedProgramTime" => {
            number(&item.value).map(MappedUpdate::TimeElapsedSecs)
        }
        "BSH.Common.Root.ActiveProgram" => short_enum(&item.value).map(MappedUpdate::ActiveProgram),
        key if key.starts_with("BSH.Common.Event.") => program_event(key),
        _ => None,
    }
}

/// Cuts the leaf label after the last dot of a string value.
/// Gauges carry the leaf only; the full enum path would repeat the same
/// prefix on every scrape.
fn short_enum(value: &Value) -> Option<String> {
    let full = value.as_str()?;
    let short = full.rsplit('.').next().unwrap_or_default();
    // An empty or dot-terminated value is malformed, not a label.
    if short.is_empty() {
        return None;
    }
    Some(short.to_owned())
}

/// Reads the door-state leaf and turns it into an open flag.
/// Any other leaf (or a non-string value) does not fit the gauge.
fn door_state(value: &Value) -> Option<MappedUpdate> {
    match short_enum(value)?.as_str() {
        "Open" => Some(MappedUpdate::DoorOpen(true)),
        "Closed" => Some(MappedUpdate::DoorOpen(false)),
        _ => None,
    }
}

/// Reads a numeric value. Only JSON numbers fit a gauge; a string or a
/// boolean would invent data the appliance never sent.
fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// Builds a one-shot program event from its key.
/// The event name rides on the key; the value only says Present or Absent.
fn program_event(key: &str) -> Option<MappedUpdate> {
    let name = key.strip_prefix("BSH.Common.Event.")?;
    if name.is_empty() {
        return None;
    }
    Some(MappedUpdate::ProgramEvent(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{EventItem, MappedUpdate, StreamEvent, classify, map_item};
    use crate::sse::{SseMessage, SseParser};
    use serde_json::json;

    const STATUS_JSON: &str = r#"{"haId":"SIEMENS-WM14T6H9NL-AB1234567890","items":[{"handling":"none","key":"BSH.Common.Status.LocalControlActive","level":"hint","timestamp":1676897835.000,"uri":"/api/...","value":true},{"handling":"none","key":"BSH.Common.Status.RemoteControlActive","level":"hint","timestamp":1676897835.000,"uri":"/api/...","value":false}]}"#;
    const EVENT_JSON: &str = r#"{"items":[{"timestamp":1642001123.000,"handling":"none","key":"BSH.Common.Event.ProgramFinished","value":"BSH.Common.EnumType.EventPresentState.Present","level":"hint"}],"haId":"SIEMENS-WM14T6H9NL-AB1234567890"}"#;
    const NOTIFY_JSON: &str = r#"{"haId":"SIEMENS-WM14T6H9NL-AB1234567890","items":[{"handling":"none","key":"BSH.Common.Root.SelectedProgram","level":"hint","timestamp":1676897836.000,"uri":"/api/...","value":"LaundryCare.Washer.Program.DelicatesSilk"}]}"#;
    const CONNECTED_JSON: &str = r#"{"haId":"SIEMENS-WT8HXM90NL-AB1234567890","handling":"none","key":"BSH.Common.Appliance.Connected","level":"hint","timestamp":1676897865.000,"value":true}"#;
    const DISCONNECTED_JSON: &str = r#"{"haId":"SIEMENS-WT8HXM90NL-AB1234567890","handling":"none","key":"BSH.Common.Appliance.Disconnected","level":"hint","timestamp":1676897866.000,"value":true}"#;

    /// Parses one wire fixture into its single message.
    fn parse_one(wire: &str) -> SseMessage {
        let mut parser = SseParser::new();
        let mut messages = parser.push(wire.as_bytes());
        assert_eq!(
            messages.len(),
            1,
            "the fixture must yield exactly one message"
        );
        messages.pop().unwrap()
    }

    /// Parses and classifies one wire fixture.
    fn classify_wire(wire: &str) -> Option<StreamEvent> {
        classify(&parse_one(wire))
    }

    /// Builds an item without going through the wire.
    fn item(key: &str, value: serde_json::Value) -> EventItem {
        EventItem {
            key: key.to_owned(),
            value,
        }
    }

    #[test]
    fn the_status_sample_becomes_items() {
        // Extra fields (handling, level, timestamp, uri) never surface.
        let wire = format!("event:STATUS\ndata: {STATUS_JSON}\n\n");
        assert_eq!(
            classify_wire(&wire),
            Some(StreamEvent::Items(vec![
                item("BSH.Common.Status.LocalControlActive", json!(true)),
                item("BSH.Common.Status.RemoteControlActive", json!(false)),
            ]))
        );
    }

    #[test]
    fn the_event_sample_maps_to_program_finished() {
        let wire = format!("event:EVENT\ndata:{EVENT_JSON}\n\n");
        let Some(StreamEvent::Items(items)) = classify_wire(&wire) else {
            unreachable!("the EVENT sample must classify as items");
        };
        assert_eq!(
            items.iter().map(map_item).collect::<Vec<_>>(),
            vec![Some(MappedUpdate::ProgramEvent(
                "ProgramFinished".to_owned()
            ))]
        );
    }

    #[test]
    fn the_notify_sample_classifies_as_items_but_maps_to_nothing() {
        let wire = format!("event:NOTIFY\ndata:{NOTIFY_JSON}\n\n");
        let Some(StreamEvent::Items(items)) = classify_wire(&wire) else {
            unreachable!("the NOTIFY sample must classify as items");
        };
        assert_eq!(
            items,
            vec![item(
                "BSH.Common.Root.SelectedProgram",
                json!("LaundryCare.Washer.Program.DelicatesSilk"),
            )]
        );
        for mapped in items.iter().map(map_item) {
            assert_eq!(mapped, None);
        }
    }

    #[test]
    fn the_connected_sample_classifies_as_connected() {
        let wire = format!("event:CONNECTED\ndata:{CONNECTED_JSON}\n\n");
        assert_eq!(classify_wire(&wire), Some(StreamEvent::Connected));
    }

    #[test]
    fn the_disconnected_sample_classifies_as_disconnected() {
        let wire = format!("event:DISCONNECTED\ndata:{DISCONNECTED_JSON}\n\n");
        assert_eq!(classify_wire(&wire), Some(StreamEvent::Disconnected));
    }

    #[test]
    fn keep_alives_are_heartbeats_with_or_without_a_data_line() {
        // The real stream sends `data:` with nothing after it.
        assert_eq!(
            classify_wire("event:KEEP-ALIVE\ndata:\n\n"),
            Some(StreamEvent::KeepAlive)
        );
        // The data line may also be absent entirely.
        let absent = SseMessage {
            event: Some("KEEP-ALIVE".to_owned()),
            data: None,
            id: None,
        };
        assert_eq!(classify(&absent), Some(StreamEvent::KeepAlive));
        // A payload-less message is a heartbeat too.
        assert_eq!(
            classify(&SseMessage::default()),
            Some(StreamEvent::KeepAlive)
        );
        // The KEEP-ALIVE name wins even when junk rides along.
        let named = SseMessage {
            event: Some("KEEP-ALIVE".to_owned()),
            data: Some("junk".to_owned()),
            id: None,
        };
        assert_eq!(classify(&named), Some(StreamEvent::KeepAlive));
    }

    #[test]
    fn paired_and_depaired_are_recognized() {
        let paired = SseMessage {
            event: Some("PAIRED".to_owned()),
            data: Some("{}".to_owned()),
            id: None,
        };
        let depaired = SseMessage {
            event: Some("DEPAIRED".to_owned()),
            data: Some("{}".to_owned()),
            id: None,
        };
        assert_eq!(classify(&paired), Some(StreamEvent::Paired));
        assert_eq!(classify(&depaired), Some(StreamEvent::Depaired));
    }

    #[test]
    fn unparseable_data_is_dropped() {
        assert_eq!(classify_wire("event:STATUS\ndata:{not json}\n\n"), None);
    }

    #[test]
    fn an_unknown_event_name_is_dropped() {
        let message = SseMessage {
            event: Some("FROBNICATE".to_owned()),
            data: Some("{}".to_owned()),
            id: None,
        };
        assert_eq!(classify(&message), None);
    }

    #[test]
    fn missing_or_empty_items_classify_as_empty_items() {
        let absent = SseMessage {
            event: Some("STATUS".to_owned()),
            data: Some(r#"{"haId":"x"}"#.to_owned()),
            id: None,
        };
        let empty = SseMessage {
            event: Some("NOTIFY".to_owned()),
            data: Some(r#"{"items":[]}"#.to_owned()),
            id: None,
        };
        assert_eq!(classify(&absent), Some(StreamEvent::Items(Vec::new())));
        assert_eq!(classify(&empty), Some(StreamEvent::Items(Vec::new())));
    }

    #[test]
    fn malformed_items_are_skipped_not_fatal() {
        let message = SseMessage {
            event: Some("STATUS".to_owned()),
            data: Some(r#"{"items":[{"key":123},"not an object"]}"#.to_owned()),
            id: None,
        };
        assert_eq!(classify(&message), Some(StreamEvent::Items(Vec::new())));
    }

    #[test]
    fn an_item_without_a_value_keeps_a_null_value() {
        let message = SseMessage {
            event: Some("NOTIFY".to_owned()),
            data: Some(r#"{"items":[{"key":"BSH.Common.Status.RemoteControlActive"}]}"#.to_owned()),
            id: None,
        };
        assert_eq!(
            classify(&message),
            Some(StreamEvent::Items(vec![item(
                "BSH.Common.Status.RemoteControlActive",
                json!(null),
            )]))
        );
    }

    #[test]
    fn operation_state_maps_to_the_short_label() {
        for (value, label) in [
            ("BSH.Common.EnumType.OperationState.Run", "Run"),
            ("BSH.Common.EnumType.OperationState.Inactive", "Inactive"),
            ("BSH.Common.EnumType.OperationState.Ready", "Ready"),
            ("BSH.Common.EnumType.OperationState.Finished", "Finished"),
        ] {
            assert_eq!(
                map_item(&item("BSH.Common.Status.OperationState", json!(value))),
                Some(MappedUpdate::OperationState(label.to_owned()))
            );
        }
    }

    #[test]
    fn non_string_operation_state_values_do_not_fit() {
        assert_eq!(
            map_item(&item("BSH.Common.Status.OperationState", json!(3))),
            None
        );
        assert_eq!(
            map_item(&item("BSH.Common.Status.OperationState", json!(true))),
            None
        );
        assert_eq!(
            map_item(&item("BSH.Common.Status.OperationState", json!(""))),
            None
        );
    }

    #[test]
    fn door_state_maps_open_and_closed() {
        assert_eq!(
            map_item(&item(
                "BSH.Common.Status.DoorState",
                json!("BSH.Common.EnumType.DoorState.Open")
            )),
            Some(MappedUpdate::DoorOpen(true))
        );
        assert_eq!(
            map_item(&item(
                "BSH.Common.Status.DoorState",
                json!("BSH.Common.EnumType.DoorState.Closed")
            )),
            Some(MappedUpdate::DoorOpen(false))
        );
    }

    #[test]
    fn other_door_states_do_not_map() {
        assert_eq!(
            map_item(&item(
                "BSH.Common.Status.DoorState",
                json!("BSH.Common.EnumType.DoorState.Reopened")
            )),
            None
        );
        assert_eq!(
            map_item(&item("BSH.Common.Status.DoorState", json!(true))),
            None
        );
    }

    #[test]
    fn numeric_options_map_to_percent_and_seconds() {
        assert_eq!(
            map_item(&item("BSH.Common.Option.ProgramProgress", json!(90))),
            Some(MappedUpdate::ProgramProgress(90.0))
        );
        assert_eq!(
            map_item(&item("BSH.Common.Option.ProgramProgress", json!(12.5))),
            Some(MappedUpdate::ProgramProgress(12.5))
        );
        assert_eq!(
            map_item(&item("BSH.Common.Option.RemainingProgramTime", json!(1800))),
            Some(MappedUpdate::TimeRemainingSecs(1800.0))
        );
        assert_eq!(
            map_item(&item("BSH.Common.Option.ElapsedProgramTime", json!(600))),
            Some(MappedUpdate::TimeElapsedSecs(600.0))
        );
    }

    #[test]
    fn non_numeric_options_do_not_map() {
        for key in [
            "BSH.Common.Option.ProgramProgress",
            "BSH.Common.Option.RemainingProgramTime",
            "BSH.Common.Option.ElapsedProgramTime",
        ] {
            // A numeric string is not a JSON number; the gauge needs the
            // real thing.
            assert_eq!(map_item(&item(key, json!("90"))), None);
            assert_eq!(map_item(&item(key, json!(null))), None);
        }
    }

    #[test]
    fn active_program_maps_to_the_short_label() {
        assert_eq!(
            map_item(&item(
                "BSH.Common.Root.ActiveProgram",
                json!("Dishcare.Dishwasher.Program.Eco50")
            )),
            Some(MappedUpdate::ActiveProgram("Eco50".to_owned()))
        );
    }

    #[test]
    fn event_keys_map_to_program_events() {
        assert_eq!(
            map_item(&item(
                "BSH.Common.Event.ProgramFinished",
                json!("BSH.Common.EnumType.EventPresentState.Present")
            )),
            Some(MappedUpdate::ProgramEvent("ProgramFinished".to_owned()))
        );
        assert_eq!(
            map_item(&item(
                "BSH.Common.Event.ProgramAborted",
                json!("BSH.Common.EnumType.EventPresentState.Present")
            )),
            Some(MappedUpdate::ProgramEvent("ProgramAborted".to_owned()))
        );
        // An event key with an empty name is malformed, not an event.
        assert_eq!(
            map_item(&item(
                "BSH.Common.Event.",
                json!("BSH.Common.EnumType.EventPresentState.Present")
            )),
            None
        );
    }

    #[test]
    fn unmapped_keys_return_none() {
        for key in [
            "BSH.Common.Status.RemoteControlActive",
            "BSH.Common.Status.LocalControlActive",
            "BSH.Common.Root.SelectedProgram",
            "BSH.Common.Root.SelectedProgramWithSuffix",
            "",
        ] {
            assert_eq!(map_item(&item(key, json!(true))), None);
        }
    }
}
