//! The embedded MQTT broker of the local path. No Docker, no external broker.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;

use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings, TlsConfig};

use crate::certs;
use crate::error::{Error, Result};

/// Port for the QUBO devices (TLS).
pub const DEVICE_PORT: u16 = 8883;
/// Default port for `qubo` commands and `qubo mcp` on this host (plain MQTT).
pub const LOCAL_PORT: u16 = 1883;

/// Settings of the local broker.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Port for the devices (TLS).
    pub device_port: u16,
    /// Address that the device listener binds to.
    pub device_host: IpAddr,
    /// Port for local tools (plain MQTT, loopback only).
    pub local_port: u16,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            device_port: DEVICE_PORT,
            device_host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            local_port: LOCAL_PORT,
        }
    }
}

fn server(name: &str, listen: SocketAddr, tls: Option<TlsConfig>) -> ServerSettings {
    ServerSettings {
        name: name.to_owned(),
        listen,
        tls,
        next_connection_delay_ms: 1,
        connections: ConnectionSettings {
            connection_timeout_ms: 60_000,
            max_payload_size: 262_144,
            max_inflight_count: 100,
            auth: None,
            external_auth: None,
            // The tools subscribe to filters such as /monitor/#.
            dynamic_filters: true,
        },
    }
}

/// Fail if something already listens on the address.
///
/// The broker library only logs a bind error inside its own thread. Without this check, a
/// second `qubo serve` would look healthy while it uses the broker of the first one.
fn ensure_free(address: SocketAddr) -> Result<()> {
    std::net::TcpListener::bind(address).map(drop).map_err(|err| {
        Error::Connection(format!("Port {} is already in use ({err}).", address.port()))
    })
}

/// A running broker. It runs in its own thread until the process ends.
#[derive(Debug)]
pub struct LocalBroker {
    failed: Mutex<mpsc::Receiver<String>>,
}

impl LocalBroker {
    /// Make the certificates if they are missing, then start the broker.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Value`] if the certificates cannot be made, and [`Error::Io`] if a file cannot be written or the broker thread cannot start.
    pub fn start(cert_dir: &Path, config: &BrokerConfig) -> Result<Self> {
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.local_port);
        let devices = SocketAddr::new(config.device_host, config.device_port);
        ensure_free(loopback)?;
        ensure_free(devices)?;
        let (crt, key) = certs::ensure(cert_dir)?;
        let tls = TlsConfig::Rustls {
            capath: None,
            certpath: crt.to_string_lossy().into_owned(),
            keypath: key.to_string_lossy().into_owned(),
        };
        let settings = Config {
            id: 0,
            router: RouterConfig {
                max_connections: 1000,
                max_outgoing_packet_count: 200,
                max_segment_size: 1_048_576,
                max_segment_count: 10,
                custom_segment: None,
                initialized_filters: None,
                shared_subscriptions_strategy: rumqttd::Strategy::default(),
            },
            v4: Some(HashMap::from([
                ("local".to_owned(), server("local", loopback, None)),
                ("devices".to_owned(), server("devices", devices, Some(tls))),
            ])),
            ..Config::default()
        };
        let (failed_tx, failed) = mpsc::channel();
        thread::Builder::new().name("qubo-broker".to_owned()).spawn(move || {
            if let Err(err) = Broker::new(settings).start() {
                let _ = failed_tx.send(err.to_string());
            }
        })?;
        Ok(Self { failed: Mutex::new(failed) })
    }

    /// The start error, if the broker stopped.
    pub fn failure(&self) -> Option<Error> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_recv()
            .ok()
            .map(|reason| Error::Connection(format!("The local broker did not start: {reason}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_on(local_port: u16, device_port: u16) -> BrokerConfig {
        BrokerConfig { local_port, device_port, device_host: IpAddr::V4(Ipv4Addr::LOCALHOST) }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    mod start {
        use super::*;

        #[test]
        fn fails_when_the_local_port_is_in_use() {
            let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = taken.local_addr().unwrap().port();
            let dir = tempfile::tempdir().unwrap();
            let result = LocalBroker::start(dir.path(), &config_on(port, free_port()));
            assert!(
                matches!(result, Err(Error::Connection(ref m)) if m.contains("already in use"))
            );
        }

        #[test]
        fn fails_when_the_device_port_is_in_use() {
            let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = taken.local_addr().unwrap().port();
            let dir = tempfile::tempdir().unwrap();
            let result = LocalBroker::start(dir.path(), &config_on(free_port(), port));
            assert!(
                matches!(result, Err(Error::Connection(ref m)) if m.contains("already in use"))
            );
        }

        #[test]
        fn makes_no_certificates_when_a_port_is_in_use() {
            let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = taken.local_addr().unwrap().port();
            let dir = tempfile::tempdir().unwrap();
            let _ = LocalBroker::start(dir.path(), &config_on(port, free_port()));
            assert!(!dir.path().join("server.crt").exists());
        }
    }
}
