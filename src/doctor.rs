//! `qubo doctor`: check the setup of both paths.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::Serialize;

use crate::broker::DEVICE_PORT;
use crate::cloud::CloudSession;
use crate::models::{Config, Mode, Paths};
use crate::protocol::CLOUD_HOST;
use crate::serve::{dns_points_here, port_open};
use crate::service::{Platform, firewall_note};

/// The result of one check.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Name of the check.
    pub check: &'static str,
    /// True if the check passed.
    pub ok: bool,
    /// What the check found, or what to do.
    pub detail: String,
}

async fn cloud_login(paths: &Paths) -> bool {
    match CloudSession::load(paths) {
        Ok(session) => session.logged_in().await,
        Err(_) => false,
    }
}

/// The path that `qubo` uses: the local broker, the cloud, or `none`.
#[must_use]
pub fn selected_path(mode: Mode, broker_up: bool, logged_in: bool) -> String {
    match mode {
        Mode::Auto if broker_up => "local".to_owned(),
        Mode::Auto if logged_in => "cloud".to_owned(),
        Mode::Auto => "none".to_owned(),
        other => other.to_string(),
    }
}

fn check(check: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check { check, ok, detail: detail.into() }
}

/// Run all checks. The last check says which path `qubo` selects.
pub async fn checks(paths: &Paths, config: &Config) -> Vec<Check> {
    let login = cloud_login(paths).await;
    let timeout = Duration::from_secs(1);
    let broker_address = format!("{}:{}", config.host, config.port);
    let broker = match tokio::net::lookup_host(&broker_address).await {
        Ok(mut found) => match found.next() {
            Some(address) => port_open(address, timeout).await,
            None => false,
        },
        Err(_) => false,
    };
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEVICE_PORT);
    let devices_port = port_open(loopback, timeout).await;
    let (here, address) = dns_points_here().await;
    let path = selected_path(config.mode, broker, login);
    let mut rows = vec![
        check("cloud login", login, if login { "saved" } else { "run 'qubo login'" }),
        check("local broker (1883)", broker, broker_address),
        check(
            "device port (8883)",
            devices_port,
            if devices_port {
                "listening"
            } else {
                "not listening; run 'qubo serve' for the local path"
            },
        ),
        check(
            "DNS record",
            here,
            format!(
                "{CLOUD_HOST} -> {address}{}",
                if here { "" } else { " (local path needs this host)" }
            ),
        ),
        check("devices", !config.devices.is_empty(), config.devices.len().to_string()),
        check("selected path", path != "none", format!("{path} (mode {})", config.mode)),
    ];
    if let Some(note) = Platform::current().ok().and_then(firewall_note) {
        // Keep "selected path" last. Its result is the exit code.
        rows.insert(rows.len() - 1, check("device port exposure", true, note));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    mod selected_path {
        use super::*;

        #[test]
        fn auto_mode_picks_local_when_the_broker_answers() {
            assert_eq!(selected_path(Mode::Auto, true, true), "local");
        }

        #[test]
        fn auto_mode_picks_cloud_when_only_a_login_exists() {
            assert_eq!(selected_path(Mode::Auto, false, true), "cloud");
        }

        #[test]
        fn auto_mode_picks_none_when_nothing_is_available() {
            assert_eq!(selected_path(Mode::Auto, false, false), "none");
        }

        #[test]
        fn a_fixed_mode_wins_over_what_is_available() {
            assert_eq!(selected_path(Mode::Cloud, true, false), "cloud");
        }
    }

    mod checks {
        use super::*;

        async fn rows() -> Vec<Check> {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path().join("config.json"));
            // A closed local port keeps the run fast and offline.
            let config = Config { port: 1, ..Config::default() };
            checks(&paths, &config).await
        }

        #[tokio::test]
        async fn ends_with_the_selected_path() {
            assert_eq!(rows().await.last().map(|r| r.check), Some("selected path"));
        }

        #[tokio::test]
        async fn fails_the_last_check_when_no_path_exists() {
            assert_eq!(rows().await.last().map(|r| r.ok), Some(false));
        }

        #[tokio::test]
        async fn reports_no_cloud_login_without_a_session_file() {
            assert!(!rows().await[0].ok);
        }
    }
}
