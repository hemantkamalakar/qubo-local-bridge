//! The local path end to end: `qubo serve` with a simulated purifier that connects over TLS,
//! and the `qubo` binary on top.

use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::Duration;

use qubo_bridge::broker::BrokerConfig;
use qubo_bridge::models::{Config, Paths};
use qubo_bridge::serve::{port_open, serve};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn report(service: &str, values: &Value) -> String {
    json!({ "devices": { "services": { service: { "events": { "stateChanged": values } } } } })
        .to_string()
}

const HEARTBEAT: &str = r#"{"devices" : {"deviceUUID" : "dp","entityUUID" : "ep","unitUUID" : "u1","userUUID" : "usr","srcDeviceId" : "HPH_94:51:DC:00:00:01","operationState" : "online","msgSequenceId" : 0001}}"#;

/// Answer commands like a purifier does.
async fn run_fake_purifier(device_port: u16, ca: Vec<u8>) {
    let mut options = MqttOptions::new("fake-hph", "localhost", device_port);
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca,
        alpn: None,
        client_auth: None,
    }));
    let (device, mut events) = AsyncClient::new(options, 32);
    device.subscribe("/control/#", QoS::AtLeastOnce).await.unwrap();

    let beats = device.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ =
                beats.publish("/monitor/u1/dp/heartbeat", QoS::AtMostOnce, false, HEARTBEAT).await;
        }
    });

    while let Ok(event) = events.poll().await {
        let Event::Incoming(Packet::Publish(message)) = event else { continue };
        let parts: Vec<&str> = message.topic.split('/').collect();
        let [_, _, unit, id, service] = parts[..] else { continue };
        let body: Value = serde_json::from_slice(&message.payload).unwrap();
        let entry = &body["command"]["devices"]["services"][service];
        let (reply_service, values) = if service == "aqiRefresh" {
            ("aqiStatus", json!({ "PM25": "33" }))
        } else if entry.get("attributes").is_some() {
            (service, entry["attributes"].clone())
        } else {
            continue;
        };
        let topic = format!("/monitor/{unit}/{id}/{reply_service}");
        let _ =
            device.publish(topic, QoS::AtLeastOnce, false, report(reply_service, &values)).await;
    }
}

/// Run the `qubo` binary with the config of a fixture. Return its output.
async fn run_qubo(paths: &Paths, args: &[&str]) -> std::process::Output {
    let config = paths.config().to_path_buf();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_qubo"))
            .env("QUBO_CONFIG_PATH", config)
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

/// Run a command that must succeed. Return the JSON that it printed.
async fn qubo(paths: &Paths, args: &[&str]) -> Value {
    let output = run_qubo(paths, args).await;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "stdout: {text}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&text).unwrap()
}

/// Wait until `qubo serve` has saved the purifier in the configuration file.
///
/// A command finds a device through this file, so the tests must wait for it.
async fn wait_for_saved_device(paths: &Paths) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while Config::load(paths).unwrap().devices.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no purifier saved in 20 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `qubo serve` on free ports, with a simulated purifier that is connected over TLS.
struct LocalPath {
    paths: Paths,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<qubo_bridge::error::Result<()>>,
    purifier: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl LocalPath {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("config.json"));
        let (local_port, device_port) = (free_port(), free_port());
        Config { port: local_port, ..Config::default() }.save(&paths).unwrap();

        let (stop, stop_rx) = oneshot::channel::<()>();
        let broker =
            BrokerConfig { device_port, device_host: IpAddr::V4(Ipv4Addr::LOCALHOST), local_port };
        let server_paths = paths.clone();
        let server = tokio::spawn(async move {
            let config = Config::load(&server_paths).unwrap();
            serve(&server_paths, config, broker, async {
                let _ = stop_rx.await;
            })
            .await
        });
        let local = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port);
        for _ in 0..100 {
            if port_open(local, Duration::from_millis(200)).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let ca = std::fs::read(paths.broker_dir().join("ca.crt")).unwrap();
        let purifier = tokio::spawn(run_fake_purifier(device_port, ca));
        wait_for_saved_device(&paths).await;
        Self { paths, stop, server, purifier, _dir: dir }
    }

    async fn stop(self) {
        self.purifier.abort();
        self.stop.send(()).unwrap();
        self.server.await.unwrap().unwrap();
    }
}

mod discover {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn finds_the_purifier_from_its_heartbeat() {
        let path = LocalPath::start().await;
        let found =
            qubo(&path.paths, &["discover", "--seconds", "0.5", "--mode", "local", "--json"]).await;
        path.stop().await;
        assert_eq!(found[0]["kind"], "HPH");
    }
}

mod purifier_power {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn confirms_a_switch_on_over_the_local_path() {
        let path = LocalPath::start().await;
        let result = qubo(&path.paths, &["purifier", "power", "on", "--json"]).await;
        path.stop().await;
        assert_eq!(
            result,
            json!({ "device": "purifier 94:51:DC:00:00:01", "path": "local", "confirmed": true })
        );
    }
}

mod status {
    use super::*;

    async fn status_row() -> Value {
        let path = LocalPath::start().await;
        qubo(&path.paths, &["purifier", "power", "on", "--json"]).await;
        let status = qubo(&path.paths, &["status", "--json"]).await;
        path.stop().await;
        status[0].clone()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reports_the_pm25_value_of_the_device() {
        assert_eq!(status_row().await["pm25"], 33);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reports_the_power_state_that_a_command_set() {
        assert_eq!(status_row().await["power"], "on");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reports_the_local_path() {
        assert_eq!(status_row().await["path"], "local");
    }
}

mod cloud_mode_without_a_login {
    use super::*;

    async fn run() -> std::process::Output {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("config.json"));
        run_qubo(&paths, &["status", "--json", "--mode", "cloud"]).await
    }

    #[tokio::test]
    async fn exits_with_a_failure_code() {
        assert!(!run().await.status.success());
    }

    #[tokio::test]
    async fn prints_a_json_error_that_names_the_missing_login() {
        let body: Value = serde_json::from_slice(&run().await.stdout).unwrap();
        assert!(body["error"].as_str().unwrap().contains("no cloud login"), "{body}");
    }
}
