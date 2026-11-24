//! Environment configuration.
//!
//! Every knob is an environment variable with the HCE_ prefix.
//! The three credentials are required; the rest have defaults.

use std::net::SocketAddr;

use crate::error::{Error, Result};

/// Runtime configuration, read once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Home Connect OAuth client id from the developer portal.
    pub client_id: String,
    /// Home Connect OAuth client secret from the developer portal.
    pub client_secret: String,
    /// OAuth refresh token obtained with the one-time authorize flow.
    pub refresh_token: String,
    /// Address the /metrics listener binds to.
    pub listen_addr: SocketAddr,
    /// Home Connect API root. Overridable so tests can point at a local mock.
    pub api_base: String,
    /// Daily budget for counted API calls (token refreshes, snapshots, stream
    /// opens). Home Connect allows 1000 per client and user per day; the
    /// default keeps 100 in reserve.
    pub daily_request_budget: i64,
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    /// Returns [`Error::MissingEnv`] or [`Error::InvalidEnv`] when a variable
    /// is absent or not parseable.
    pub fn from_env() -> Result<Self> {
        Self::build([
            (
                "HCE_CLIENT_ID",
                std::env::var("HCE_CLIENT_ID").ok().as_deref(),
            ),
            (
                "HCE_CLIENT_SECRET",
                std::env::var("HCE_CLIENT_SECRET").ok().as_deref(),
            ),
            (
                "HCE_REFRESH_TOKEN",
                std::env::var("HCE_REFRESH_TOKEN").ok().as_deref(),
            ),
            (
                "HCE_LISTEN_ADDR",
                std::env::var("HCE_LISTEN_ADDR").ok().as_deref(),
            ),
            (
                "HCE_API_BASE",
                std::env::var("HCE_API_BASE").ok().as_deref(),
            ),
            (
                "HCE_DAILY_REQUEST_BUDGET",
                std::env::var("HCE_DAILY_REQUEST_BUDGET").ok().as_deref(),
            ),
        ])
    }

    /// Reads the credentials for the one-time authorize flow.
    /// The refresh token is not needed here: this flow is how it is obtained.
    /// The listener address is the local redirect endpoint, not the scrape
    /// listener.
    ///
    /// # Errors
    /// Returns an error when a required credential is missing.
    pub fn from_env_authorize() -> Result<Self> {
        Self::build_authorize([
            (
                "HCE_CLIENT_ID",
                std::env::var("HCE_CLIENT_ID").ok().as_deref(),
            ),
            (
                "HCE_CLIENT_SECRET",
                std::env::var("HCE_CLIENT_SECRET").ok().as_deref(),
            ),
            (
                "HCE_LISTEN_ADDR",
                std::env::var("HCE_LISTEN_ADDR").ok().as_deref(),
            ),
            (
                "HCE_API_BASE",
                std::env::var("HCE_API_BASE").ok().as_deref(),
            ),
            (
                "HCE_DAILY_REQUEST_BUDGET",
                std::env::var("HCE_DAILY_REQUEST_BUDGET").ok().as_deref(),
            ),
        ])
    }

    /// Like [`Config::build`], but the refresh token starts empty: the
    /// authorize flow creates it, and the listener default is the local
    /// redirect endpoint.
    fn build_authorize<'a>(
        entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> Result<Self> {
        let mut values: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (name, value) in entries {
            if let Some(value) = value {
                values.insert(name, value);
            }
        }
        Ok(Self {
            client_id: required(&values, "HCE_CLIENT_ID")?,
            client_secret: required(&values, "HCE_CLIENT_SECRET")?,
            refresh_token: String::new(),
            listen_addr: parse(&values, "HCE_LISTEN_ADDR", "127.0.0.1:7878")?,
            api_base: parse(&values, "HCE_API_BASE", "https://api.home-connect.com")?,
            daily_request_budget: parse(&values, "HCE_DAILY_REQUEST_BUDGET", "900")?,
        })
    }

    /// Builds the configuration from name-value pairs.
    /// A `None` value means the variable is not set; the pair order is not
    /// significant.
    ///
    /// # Errors
    /// Returns [`Error::MissingEnv`] for a missing required variable and
    /// [`Error::InvalidEnv`] for a value that does not parse.
    pub fn build<'a>(
        entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> Result<Self> {
        let mut values: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (name, value) in entries {
            if let Some(value) = value {
                values.insert(name, value);
            }
        }
        Ok(Self {
            client_id: required(&values, "HCE_CLIENT_ID")?,
            client_secret: required(&values, "HCE_CLIENT_SECRET")?,
            refresh_token: required(&values, "HCE_REFRESH_TOKEN")?,
            listen_addr: parse(&values, "HCE_LISTEN_ADDR", "0.0.0.0:9780")?,
            api_base: parse(&values, "HCE_API_BASE", "https://api.home-connect.com")?,
            daily_request_budget: parse(&values, "HCE_DAILY_REQUEST_BUDGET", "900")?,
        })
    }
}

/// Reads a required variable, or fails with its name.
fn required(values: &std::collections::HashMap<&str, &str>, name: &str) -> Result<String> {
    values
        .get(name)
        .copied()
        .map(str::to_owned)
        .ok_or_else(|| Error::MissingEnv(name.to_owned()))
}

/// Reads a variable with a default, then parses it.
fn parse<T>(values: &std::collections::HashMap<&str, &str>, name: &str, default: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    let raw = values.get(name).copied().unwrap_or(default);
    raw.parse::<T>().map_err(|_| Error::InvalidEnv {
        name: name.to_owned(),
        message: format!("cannot parse {raw:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn missing_client_id_is_reported() {
        let entries: [(&str, Option<&str>); 0] = [];
        let err = Config::build(entries).expect_err("config must fail");
        assert_eq!(
            err.to_string(),
            "missing required environment variable: HCE_CLIENT_ID"
        );
    }

    #[test]
    fn defaults_fill_the_optional_values() {
        let entries = [
            ("HCE_CLIENT_ID", Some("id")),
            ("HCE_CLIENT_SECRET", Some("secret")),
            ("HCE_REFRESH_TOKEN", Some("refresh")),
        ];
        let config = Config::build(entries).expect("config must parse");
        assert_eq!(config.listen_addr.to_string(), "0.0.0.0:9780");
        assert_eq!(config.api_base, "https://api.home-connect.com");
        assert_eq!(config.daily_request_budget, 900);
    }

    #[test]
    fn overrides_are_honoured() {
        let entries = [
            ("HCE_CLIENT_ID", Some("id")),
            ("HCE_CLIENT_SECRET", Some("secret")),
            ("HCE_REFRESH_TOKEN", Some("refresh")),
            ("HCE_LISTEN_ADDR", Some("127.0.0.1:9999")),
            ("HCE_API_BASE", Some("http://127.0.0.1:1")),
            ("HCE_DAILY_REQUEST_BUDGET", Some("7")),
        ];
        let config = Config::build(entries).expect("config must parse");
        assert_eq!(config.listen_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(config.api_base, "http://127.0.0.1:1");
        assert_eq!(config.daily_request_budget, 7);
    }

    #[test]
    fn an_unparseable_value_names_its_variable() {
        let entries = [
            ("HCE_CLIENT_ID", Some("id")),
            ("HCE_CLIENT_SECRET", Some("secret")),
            ("HCE_REFRESH_TOKEN", Some("refresh")),
            ("HCE_LISTEN_ADDR", Some("not-a-socket")),
        ];
        let err = Config::build(entries).expect_err("config must fail");
        assert_eq!(
            err.to_string(),
            "invalid environment variable HCE_LISTEN_ADDR: cannot parse \"not-a-socket\""
        );
    }
}
