//! `qubo serve`: the local path in one process.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::{TcpStream, UdpSocket, lookup_host};
use tracing::warn;

use crate::broker::{BrokerConfig, LocalBroker};
use crate::client::{ClientConfig, QuboClient};
use crate::error::{Error, Result};
use crate::models::{Config, Device, Paths};
use crate::protocol::CLOUD_HOST;

const BROKER_START_TIMEOUT: Duration = Duration::from_secs(15);
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

fn host_name() -> Option<String> {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// The IPv4 addresses of this host.
pub async fn host_addresses() -> Vec<IpAddr> {
    let mut addresses = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    // A UDP connect sends no packet. It only selects the LAN interface.
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await
        && socket.connect("192.0.2.1:9").await.is_ok()
        && let Ok(local) = socket.local_addr()
    {
        addresses.push(local.ip());
    }
    if let Some(name) = host_name()
        && let Ok(found) = lookup_host((name.as_str(), 0)).await
    {
        addresses.extend(found.map(|a| a.ip()).filter(IpAddr::is_ipv4));
    }
    addresses
}

/// Return whether the cloud host name resolves to this host, and the address that it gives.
pub async fn dns_points_here() -> (bool, String) {
    let address = match lookup_host((CLOUD_HOST, 0)).await {
        Ok(found) => found.map(|a| a.ip()).find(IpAddr::is_ipv4),
        Err(err) => return (false, format!("no answer ({err})")),
    };
    match address {
        Some(address) => (host_addresses().await.contains(&address), address.to_string()),
        None => (false, "no answer (no IPv4 address)".to_owned()),
    }
}

/// Return whether a TCP connection to the address succeeds.
pub async fn port_open(address: SocketAddr, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, TcpStream::connect(address)).await, Ok(Ok(_)))
}

async fn wait_for_broker(broker: &LocalBroker, port: u16) -> Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = tokio::time::Instant::now() + BROKER_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Some(err) = broker.failure() {
            return Err(err);
        }
        if port_open(address, Duration::from_millis(500)).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(Error::Connection("The local broker did not start in time.".to_owned()))
}

/// Add the found devices to the configuration file. Return true if the file changed.
///
/// The file is read again first, so that edits made while `qubo serve` runs are kept.
///
/// # Errors
///
/// Returns [`Error::Json`] or [`Error::Io`] if the file cannot be read or written.
pub fn persist_devices(paths: &Paths, found: Vec<Device>) -> Result<bool> {
    let mut config = Config::load(paths)?;
    let changed = config.merge(found);
    if changed {
        config.save(paths)?;
    }
    Ok(changed)
}

/// Run the broker, the state store, the automatic refresh and the cache until `stop` completes.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the broker or the client cannot start, and [`Error::Io`] if the configuration cannot be saved.
pub async fn serve(
    paths: &Paths,
    config: Config,
    broker_config: BrokerConfig,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let broker_config = BrokerConfig { local_port: config.port, ..broker_config };
    let broker = LocalBroker::start(&paths.broker_dir(), &broker_config)?;
    wait_for_broker(&broker, config.port).await?;
    let client_config =
        ClientConfig::local("127.0.0.1", config.port, config.devices.clone(), Some(paths.state()));
    let client = QuboClient::connect(client_config, Duration::from_secs(5)).await?;
    client.start_auto_refresh();
    let (here, address) = dns_points_here().await;
    if !here {
        warn!(
            "{CLOUD_HOST} resolves to {address}, not to this host. Add the DNS record in the router."
        );
    }
    tokio::pin!(stop);
    let mut ticker = tokio::time::interval(SAVE_INTERVAL);
    loop {
        tokio::select! {
            () = &mut stop => break,
            _ = ticker.tick() => {
                if let Err(err) = persist_devices(paths, client.devices()) {
                    warn!("Cannot save the devices: {err}");
                }
            }
        }
    }
    client.stop().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DeviceKind;

    fn device(name: &str, online: bool) -> Device {
        Device {
            kind: DeviceKind::Socket,
            mac: "mac".to_owned(),
            device_uuid: "s1".to_owned(),
            entity_uuid: "e".to_owned(),
            unit_uuid: "u".to_owned(),
            user_uuid: "x".to_owned(),
            online,
            name: name.to_owned(),
        }
    }

    fn paths_with(devices: Vec<Device>) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("config.json"));
        Config { devices, ..Config::default() }.save(&paths).unwrap();
        (dir, paths)
    }

    mod persist_devices {
        use super::*;

        #[test]
        fn keeps_a_name_that_was_edited_in_the_file() {
            let (_dir, paths) = paths_with(vec![device("renamed", true)]);
            persist_devices(&paths, vec![device("", false)]).unwrap();
            assert_eq!(Config::load(&paths).unwrap().devices[0].name, "renamed");
        }

        #[test]
        fn saves_the_changed_state() {
            let (_dir, paths) = paths_with(vec![device("desk", true)]);
            persist_devices(&paths, vec![device("", false)]).unwrap();
            assert!(!Config::load(&paths).unwrap().devices[0].online);
        }

        #[test]
        fn reports_a_change_for_a_new_device() {
            let (_dir, paths) = paths_with(vec![]);
            assert!(persist_devices(&paths, vec![device("", true)]).unwrap());
        }

        #[test]
        fn reports_no_change_for_the_same_device() {
            let (_dir, paths) = paths_with(vec![device("desk", true)]);
            assert!(!persist_devices(&paths, vec![device("", true)]).unwrap());
        }

        #[test]
        fn fails_when_the_file_is_not_valid_json() {
            let (_dir, paths) = paths_with(vec![]);
            std::fs::write(paths.config(), "{not json").unwrap();
            assert!(persist_devices(&paths, vec![device("", true)]).is_err());
        }
    }
}
