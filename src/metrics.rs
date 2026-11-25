//! Prometheus registry and every metric family this exporter serves.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::api::Appliance;
use crate::error::{Error, Result};

/// Label set shared by every family: a static key paired with a runtime value.
type Labels = Vec<(&'static str, String)>;

/// One-appliance label set.
fn appliance_label(appliance: &str) -> Labels {
    vec![("appliance", appliance.to_owned())]
}

/// Locks a mutex, treating a poisoned lock as healthy again.
/// Metric state is best-effort: a panicked holder must not take the whole
/// exporter down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Owns the registry and all families. Cheap to share through `Arc`.
pub struct Metrics {
    /// Guards the registry tree while a scrape is being encoded.
    registry: Mutex<Registry>,
    /// Identity of one appliance, pinned to the value 1.
    appliance_info: Family<Labels, Gauge>,
    /// Cloud connectivity of one appliance.
    connected: Family<Labels, Gauge>,
    /// One gauge per known state; 1 marks the state the appliance is in.
    operation_state: Family<Labels, Gauge>,
    /// Door position of one appliance.
    door_open: Family<Labels, Gauge>,
    /// Program progress in percent.
    program_progress: Family<Labels, Gauge<f64, AtomicU64>>,
    /// Remaining program runtime in seconds.
    time_remaining: Family<Labels, Gauge<f64, AtomicU64>>,
    /// Elapsed program runtime in seconds.
    time_elapsed: Family<Labels, Gauge<f64, AtomicU64>>,
    /// One gauge per known program; 1 marks the running program.
    program_active: Family<Labels, Gauge>,
    /// Program lifecycle events, counted per occurrence.
    program_events: Family<Labels, Counter>,
    /// Exporter liveness: 1 once the appliance list was fetched.
    up: Gauge,
    /// Successful Home Connect calls by call name.
    api_requests: Family<Labels, Counter>,
    /// Failed Home Connect calls by call name.
    api_errors: Family<Labels, Counter>,
    /// Current operation state per appliance, so the superseded gauge can be flipped back to 0.
    operation_states: Mutex<HashMap<String, String>>,
    /// Current program per appliance, same flip-back purpose.
    active_programs: Mutex<HashMap<String, String>>,
}

/// The empty registry equals a freshly constructed one.
impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Creates a registry with every family registered and all gauges empty.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let appliance_info = Family::default();
        registry.register(
            "homeconnect_appliance_info",
            "Static appliance identity: haId, name, brand and type; the value is always 1",
            appliance_info.clone(),
        );

        let connected = Family::default();
        registry.register(
            "homeconnect_connected",
            "1 when the appliance is connected to the Home Connect cloud",
            connected.clone(),
        );

        let operation_state = Family::default();
        registry.register(
            "homeconnect_operation_state",
            "Operation state per appliance; the gauge of the current state is 1",
            operation_state.clone(),
        );

        let door_open = Family::default();
        registry.register(
            "homeconnect_door_open",
            "1 while the appliance door is open",
            door_open.clone(),
        );

        let program_progress = Family::default();
        registry.register(
            "homeconnect_program_progress_percent",
            "Progress of the running program in percent",
            program_progress.clone(),
        );

        let time_remaining = Family::default();
        registry.register(
            "homeconnect_program_time_remaining_seconds",
            "Remaining runtime of the running program in seconds",
            time_remaining.clone(),
        );

        let time_elapsed = Family::default();
        registry.register(
            "homeconnect_program_time_elapsed_seconds",
            "Elapsed runtime of the running program in seconds",
            time_elapsed.clone(),
        );

        let program_active = Family::default();
        registry.register(
            "homeconnect_program_active",
            "Program per appliance; the gauge of the running program is 1",
            program_active.clone(),
        );

        let program_events = Family::default();
        // The encoder appends `_total` to counters, so the registered name
        // must omit it to expose `homeconnect_program_events_total`.
        registry.register(
            "homeconnect_program_events",
            "Program lifecycle events per appliance, e.g. ProgramFinished",
            program_events.clone(),
        );

        let up = Gauge::default();
        registry.register(
            "homeconnect_up",
            "1 after the appliance list was fetched, 0 before that or on failure",
            up.clone(),
        );

        let api_requests = Family::default();
        registry.register(
            "homeconnect_api_requests",
            "Home Connect API requests by call: token, appliances, status, events",
            api_requests.clone(),
        );

        let api_errors = Family::default();
        registry.register(
            "homeconnect_api_errors",
            "Failed Home Connect API calls by call; a rising rate signals auth or quota trouble",
            api_errors.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            appliance_info,
            connected,
            operation_state,
            door_open,
            program_progress,
            time_remaining,
            time_elapsed,
            program_active,
            program_events,
            up,
            api_requests,
            api_errors,
            operation_states: Mutex::new(HashMap::new()),
            active_programs: Mutex::new(HashMap::new()),
        }
    }

    /// Registers the info family entry for one appliance and seeds its
    /// connected gauge at 0.
    pub fn register_appliance(&self, appliance: &Appliance) {
        self.appliance_info
            .get_or_create(&vec![
                ("ha_id", appliance.ha_id.clone()),
                ("name", appliance.name.clone()),
                ("brand", appliance.brand.clone()),
                ("type", appliance.appliance_type.clone()),
            ])
            .set(1);
        // The stream loop labels an appliance by name, or by its haId when
        // the name is empty, so the seed has to use the same fallback.
        let label = if appliance.name.is_empty() {
            &appliance.ha_id
        } else {
            &appliance.name
        };
        self.connected.get_or_create(&appliance_label(label)).set(0);
    }

    /// Sets the appliance cloud-connection gauge.
    pub fn set_connected(&self, appliance: &str, connected: bool) {
        self.connected
            .get_or_create(&appliance_label(appliance))
            .set(i64::from(connected));
    }

    /// Sets the operation-state gauge for `state` to 1 and the previous
    /// state of the same appliance to 0.
    pub fn set_operation_state(&self, appliance: &str, state: &str) {
        // `insert` returns the previous state, so the map lock covers one
        // hash operation and nothing else.
        let previous = lock(&self.operation_states).insert(appliance.to_owned(), state.to_owned());
        if let Some(previous) = previous {
            self.operation_state
                .get_or_create(&vec![
                    ("appliance", appliance.to_owned()),
                    ("state", previous),
                ])
                .set(0);
        }
        self.operation_state
            .get_or_create(&vec![
                ("appliance", appliance.to_owned()),
                ("state", state.to_owned()),
            ])
            .set(1);
    }

    /// Sets the door-open gauge.
    pub fn set_door_open(&self, appliance: &str, open: bool) {
        self.door_open
            .get_or_create(&appliance_label(appliance))
            .set(i64::from(open));
    }

    /// Sets the program progress gauge, in percent.
    pub fn set_program_progress(&self, appliance: &str, percent: f64) {
        self.program_progress
            .get_or_create(&appliance_label(appliance))
            .set(percent);
    }

    /// Sets the remaining-program-time gauge, in seconds.
    pub fn set_time_remaining(&self, appliance: &str, seconds: f64) {
        self.time_remaining
            .get_or_create(&appliance_label(appliance))
            .set(seconds);
    }

    /// Sets the elapsed-program-time gauge, in seconds.
    pub fn set_time_elapsed(&self, appliance: &str, seconds: f64) {
        self.time_elapsed
            .get_or_create(&appliance_label(appliance))
            .set(seconds);
    }

    /// Sets the active-program gauge for `program` to 1 and the previous
    /// program of the same appliance to 0.
    pub fn set_active_program(&self, appliance: &str, program: &str) {
        // Same flip-back pattern as `set_operation_state`.
        let previous = lock(&self.active_programs).insert(appliance.to_owned(), program.to_owned());
        if let Some(previous) = previous {
            self.program_active
                .get_or_create(&vec![
                    ("appliance", appliance.to_owned()),
                    ("program", previous),
                ])
                .set(0);
        }
        self.program_active
            .get_or_create(&vec![
                ("appliance", appliance.to_owned()),
                ("program", program.to_owned()),
            ])
            .set(1);
    }

    /// Increments the program-event counter, e.g. `ProgramFinished`.
    pub fn count_program_event(&self, appliance: &str, event: &str) {
        self.program_events
            .get_or_create(&vec![
                ("appliance", appliance.to_owned()),
                ("event", event.to_owned()),
            ])
            .inc();
    }

    /// Increments the API-request counter for a call name: `token`,
    /// `appliances`, `status`, `events`.
    pub fn count_api_request(&self, call: &str) {
        self.api_requests
            .get_or_create(&vec![("call", call.to_owned())])
            .inc();
    }

    /// Increments the API-error counter for a call name.
    pub fn count_api_error(&self, call: &str) {
        self.api_errors
            .get_or_create(&vec![("call", call.to_owned())])
            .inc();
    }

    /// Sets the exporter up gauge: 1 after the appliance list succeeded.
    pub fn set_up(&self, up: bool) {
        self.up.set(i64::from(up));
    }

    /// Renders the registry in the Prometheus text exposition format.
    ///
    /// # Errors
    /// Returns an error when encoding fails. The crate has no dedicated
    /// encoding variant, so the failure maps onto the closest one, `Io`.
    pub fn encode(&self) -> Result<String> {
        let registry = lock(&self.registry);
        let mut buffer = String::new();
        encode(&mut buffer, &registry).map_err(|err| Error::Io(std::io::Error::other(err)))?;
        Ok(buffer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Metrics;
    use crate::api::Appliance;

    /// The appliance every assertion is written against.
    fn appliance() -> Appliance {
        Appliance {
            ha_id: "SIEMENS-SN678D10TG-AB1234567890".to_owned(),
            name: "Keuken".to_owned(),
            brand: "SIEMENS".to_owned(),
            appliance_type: "Dishwasher".to_owned(),
            connected: false,
        }
    }

    #[test]
    fn registration_seeds_the_info_line_and_a_zeroed_connected_gauge() {
        let metrics = Metrics::new();
        metrics.register_appliance(&appliance());
        let text = metrics.encode().unwrap();
        assert!(text.contains(
            r#"homeconnect_appliance_info{ha_id="SIEMENS-SN678D10TG-AB1234567890",name="Keuken",brand="SIEMENS",type="Dishwasher"} 1"#
        ));
        assert!(text.contains(r#"homeconnect_connected{appliance="Keuken"} 0"#));
    }

    #[test]
    fn an_empty_name_seeds_the_connected_gauge_under_the_ha_id() {
        let metrics = Metrics::new();
        let mut unnamed = appliance();
        unnamed.name.clear();
        metrics.register_appliance(&unnamed);
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(
                r#"homeconnect_connected{appliance="SIEMENS-SN678D10TG-AB1234567890"} 0"#
            )
        );
    }

    #[test]
    fn every_setter_leaves_its_line_in_the_exposition() {
        let metrics = Metrics::new();
        metrics.register_appliance(&appliance());
        metrics.set_connected("Keuken", true);
        metrics.set_operation_state("Keuken", "BSH.Common.EnumType.OperationState.Run");
        metrics.set_operation_state("Keuken", "BSH.Common.EnumType.OperationState.Finished");
        metrics.set_door_open("Keuken", true);
        metrics.set_program_progress("Keuken", 42.5);
        metrics.set_time_remaining("Keuken", 1800.0);
        metrics.set_time_elapsed("Keuken", 900.0);
        metrics.set_active_program("Keuken", "Dishcare.Dishwasher.Program.Eco50");
        metrics.set_active_program("Keuken", "Dishcare.Dishwasher.Program.QuickPower65");
        metrics.count_program_event("Keuken", "BSH.Common.Event.ProgramFinished");
        metrics.count_api_request("status");
        metrics.count_api_error("token");
        metrics.set_up(true);
        let text = metrics.encode().unwrap();

        assert!(text.contains(r#"homeconnect_connected{appliance="Keuken"} 1"#));
        assert!(text.contains(
            r#"homeconnect_operation_state{appliance="Keuken",state="BSH.Common.EnumType.OperationState.Run"} 0"#
        ));
        assert!(text.contains(
            r#"homeconnect_operation_state{appliance="Keuken",state="BSH.Common.EnumType.OperationState.Finished"} 1"#
        ));
        assert!(text.contains(r#"homeconnect_door_open{appliance="Keuken"} 1"#));
        assert!(text.contains(r#"homeconnect_program_progress_percent{appliance="Keuken"} 42.5"#));
        assert!(
            text.contains(r#"homeconnect_program_time_remaining_seconds{appliance="Keuken"} 1800"#)
        );
        assert!(
            text.contains(r#"homeconnect_program_time_elapsed_seconds{appliance="Keuken"} 900"#)
        );
        assert!(text.contains(
            r#"homeconnect_program_active{appliance="Keuken",program="Dishcare.Dishwasher.Program.Eco50"} 0"#
        ));
        assert!(text.contains(
            r#"homeconnect_program_active{appliance="Keuken",program="Dishcare.Dishwasher.Program.QuickPower65"} 1"#
        ));
        assert!(text.contains(
            r#"homeconnect_program_events_total{appliance="Keuken",event="BSH.Common.Event.ProgramFinished"} 1"#
        ));
        assert!(text.contains(r#"homeconnect_api_requests_total{call="status"} 1"#));
        assert!(text.contains(r#"homeconnect_api_errors_total{call="token"} 1"#));
        assert!(text.contains("homeconnect_up 1"));
    }
}
