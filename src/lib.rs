//! Crate root: one module per concern.
//!
//! - [`config`] reads the environment.
//! - [`oauth`] keeps the Home Connect access token fresh.
//! - [`api`] lists appliances and reads a status snapshot.
//! - [`sse`] parses Server-Sent Events byte streams.
//! - [`events`] maps Home Connect items onto typed updates.
//! - [`metrics`] owns the Prometheus registry and all metric families.
//! - [`stream`] runs one follow loop per appliance.
//! - [`server`] serves /metrics and /healthz.

pub mod api;
pub mod config;
pub mod error;
pub mod events;
pub mod metrics;
pub mod oauth;
pub mod server;
pub mod sse;
pub mod stream;
