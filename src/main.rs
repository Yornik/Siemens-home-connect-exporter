//! hce: the Home Connect Prometheus exporter.
//!
//! Startup order: logging, config, metrics, token, appliance list, one
//! follow task per appliance, then the HTTP listener. A missing credential
//! is a startup error: the process logs it and exits non-zero.
//!
//! `hce authorize` runs the one-time OAuth onboarding instead: it prints
//! the authorization URL, waits on a local redirect listener, and prints
//! the refresh token for the environment.

use std::sync::Arc;

use siemens_home_connect_exporter::api;
use siemens_home_connect_exporter::config::Config;
use siemens_home_connect_exporter::error::{Error, Result};
use siemens_home_connect_exporter::metrics::Metrics;
use siemens_home_connect_exporter::oauth::{self, TokenManager};
use siemens_home_connect_exporter::server;
use siemens_home_connect_exporter::stream::{self, Budget};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::Io);
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(err) => {
            error!("{err}");
            std::process::exit(1);
        }
    };
    let authorize = std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "authorize");
    let result = if authorize {
        runtime.block_on(authorize_flow())
    } else {
        runtime.block_on(run())
    };
    if let Err(err) = result {
        error!("{err}");
        std::process::exit(1);
    }
}

/// One-time onboarding: print the URL, catch the redirect, exchange the
/// code, print the refresh token.
async fn authorize_flow() -> Result<()> {
    let config = Config::from_env_authorize()?;
    let client = reqwest::Client::builder().build()?;
    let listen = config.listen_addr;
    info!(redirect = %listen, "starting the authorize flow");
    let refresh = oauth::authorize(&client, &config, listen).await?;
    info!("authorization complete");
    info!("set HCE_REFRESH_TOKEN to this value, then start hce without arguments:");
    info!("{refresh}");
    Ok(())
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let metrics = Arc::new(Metrics::new());
    let budget = Arc::new(Budget::new(config.daily_request_budget));
    let client = reqwest::Client::builder().build()?;
    let tokens = Arc::new(TokenManager::new(
        &config,
        client.clone(),
        metrics.clone(),
        budget.clone(),
    ));

    let token = tokens.get().await?;
    metrics.count_api_request("appliances");
    let appliances = api::list_appliances(&client, &config.api_base, &token).await?;
    if appliances.is_empty() {
        warn!("the Home Connect account has no appliances; serving base metrics only");
    }
    metrics.set_up(true);
    for appliance in &appliances {
        metrics.register_appliance(appliance);
        // Seed the cloud-connection gauge from the appliance list; every
        // later change arrives over the event stream.
        metrics.set_connected(&stream::display_name(appliance), appliance.connected);
    }
    for appliance in appliances {
        let task = stream::run(
            client.clone(),
            config.clone(),
            tokens.clone(),
            metrics.clone(),
            budget.clone(),
            appliance,
        );
        tokio::spawn(task);
    }

    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, "listening for scrape requests");
    server::serve(listener, metrics).await?;
    Ok(())
}
