//! MQTT client that keeps the latest reported state of every QUBO device.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Outgoing, Packet, QoS, Transport};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::error::{Error, Result};
use crate::models::{Device, DeviceKind, write_atomic};
use crate::protocol::{self, CLOUD_HOST, Command, MONITOR_ALL, Service};

/// A switch-off above this load needs `force`.
pub const POWER_LIMIT_W: f64 = 15.0;
/// Port of the QUBO cloud broker.
pub const CLOUD_PORT: u16 = 8883;
const CACHE_SAVE_INTERVAL: Duration = Duration::from_secs(30);
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The control path that a client uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ControlPath {
    /// The broker of `qubo serve`.
    Local,
    /// The QUBO cloud broker.
    Cloud,
}

impl ControlPath {
    /// The name in output rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
        }
    }
}

impl fmt::Display for ControlPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A future that gives an access token.
pub type TokenFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
/// A function that gives a valid access token. The client calls it before each connect.
pub type TokenProvider = Arc<dyn Fn() -> TokenFuture + Send + Sync>;

/// Broker credentials. The password is a token that the provider renews.
#[derive(Clone)]
pub struct Credentials {
    /// MQTT user name.
    pub username: String,
    /// Source of the MQTT password.
    pub token: TokenProvider,
}

/// Settings of a client.
#[derive(Clone)]
pub struct ClientConfig {
    host: String,
    port: u16,
    devices: Vec<Device>,
    cache: Option<PathBuf>,
    tls: bool,
    credentials: Option<Credentials>,
    topics: Vec<String>,
    path: ControlPath,
}

impl ClientConfig {
    /// A client for a plain-MQTT broker on this host or network.
    ///
    /// # Examples
    ///
    /// ```
    /// use qubo_bridge::client::{ClientConfig, ControlPath};
    ///
    /// // Plain MQTT on this host. The broker of `qubo serve` listens here.
    /// let config = ClientConfig::local("127.0.0.1", 1883, Vec::new(), None);
    /// # let _ = config;
    /// assert_eq!(ControlPath::Local.as_str(), "local");
    /// ```
    pub fn local(
        host: impl Into<String>,
        port: u16,
        devices: Vec<Device>,
        cache: Option<PathBuf>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            devices,
            cache,
            tls: false,
            credentials: None,
            topics: vec![MONITOR_ALL.to_owned()],
            path: ControlPath::Local,
        }
    }

    /// A client for the QUBO cloud broker.
    #[must_use]
    pub fn cloud(devices: Vec<Device>, cache: Option<PathBuf>, credentials: Credentials) -> Self {
        let mut units: Vec<&str> = devices.iter().map(|d| d.unit_uuid.as_str()).collect();
        units.sort_unstable();
        units.dedup();
        let topics = units.iter().map(|unit| format!("/monitor/{unit}/#")).collect();
        Self {
            host: CLOUD_HOST.to_owned(),
            port: CLOUD_PORT,
            devices,
            cache,
            tls: true,
            credentials: Some(credentials),
            topics,
            path: ControlPath::Cloud,
        }
    }
}

/// The telemetry fields that each service fills.
fn fields(service: &str) -> &'static [&'static str] {
    match service {
        "lcSwitchControl" => &["power"],
        "fanSpeedControl" => &["speed"],
        "fanControlMode" => &["mode"],
        "aqiStatus" => &["pm25"],
        "filterReset" => &["filter_hours"],
        "plugMetering" => &["watts", "volts", "amps", "kwh"],
        _ => &[],
    }
}

/// Seconds between automatic refreshes, by reply service.
const fn refresh_interval(reply: Service) -> Duration {
    Duration::from_secs(match reply {
        Service::Aqi => 30,
        Service::Filter => 3600,
        _ => 60,
    })
}

type Values = HashMap<String, HashMap<String, Map<String, Value>>>;

#[derive(Default)]
struct State {
    devices: Vec<Device>,
    values: Values,
    /// Tick of the last live report for each (device, service).
    updated: HashMap<(String, String), u64>,
    next_refresh: HashMap<(String, String), Duration>,
    dirty: bool,
}

fn key(device_uuid: &str, service: &str) -> (String, String) {
    (device_uuid.to_owned(), service.to_owned())
}

struct Shared {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    clock: AtomicU64,
    cache: Option<PathBuf>,
}

impl Shared {
    fn new(devices: Vec<Device>, cache: Option<PathBuf>) -> Self {
        let state = State { devices, values: load_cache(cache.as_deref()), ..State::default() };
        Self {
            state: Mutex::new(state),
            changed: watch::channel(0).0,
            clock: AtomicU64::new(0),
            cache,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A logical time. A later event always has a higher tick.
    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn notify(&self) {
        self.changed.send_modify(|count| *count += 1);
    }

    fn ingest(&self, topic: &str, payload: &[u8]) {
        let Some((_, device_uuid, service)) = protocol::parse_topic(topic) else {
            return;
        };
        {
            let mut state = self.lock();
            if service == Service::Heartbeat.as_str() {
                let Some(mut device) = protocol::parse_heartbeat(payload) else {
                    return;
                };
                match state.devices.iter_mut().find(|d| d.device_uuid == device.device_uuid) {
                    Some(known) => {
                        device.name.clone_from(&known.name);
                        *known = device;
                    }
                    None => state.devices.push(device),
                }
            } else {
                let values = protocol::reported_values(payload, service);
                if values.is_empty() {
                    return;
                }
                state
                    .values
                    .entry(device_uuid.to_owned())
                    .or_default()
                    .entry(service.to_owned())
                    .or_default()
                    .extend(values);
                let tick = self.tick();
                state.updated.insert(key(device_uuid, service), tick);
                state.dirty = true;
            }
        }
        self.notify();
    }

    fn due(&self, now: Duration) -> Vec<Command> {
        let mut state = self.lock();
        let devices = state.devices.clone();
        let mut due = Vec::new();
        for command in devices.iter().flat_map(protocol::refresh_commands) {
            let slot = key(&command.device_uuid, command.reply.as_str());
            if now >= state.next_refresh.get(&slot).copied().unwrap_or_default() {
                state.next_refresh.insert(slot, now + refresh_interval(command.reply));
                due.push(command);
            }
        }
        drop(state);
        due
    }

    fn save_cache(&self) {
        let Some(path) = &self.cache else {
            return;
        };
        let data = {
            let mut state = self.lock();
            state.dirty = false;
            serde_json::to_string(&state.values)
        };
        let saved =
            data.map_err(Error::from).and_then(|text| write_atomic(path, text.as_bytes(), false));
        if let Err(err) = saved {
            warn!("Cannot save the state cache {}: {err}", path.display());
        }
    }

    fn is_dirty(&self) -> bool {
        self.lock().dirty
    }
}

/// Load the last values from an earlier session. A bad file is ignored.
fn load_cache(path: Option<&std::path::Path>) -> Values {
    let parsed = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let Some(Value::Object(devices)) = parsed else {
        return Values::new();
    };
    devices
        .into_iter()
        .filter_map(|(device_uuid, services)| {
            let services = services.as_object()?;
            let services = services
                .iter()
                .filter_map(|(service, values)| {
                    Some((service.clone(), values.as_object()?.clone()))
                })
                .collect();
            Some((device_uuid, services))
        })
        .collect()
}

fn publish_error(err: impl fmt::Display) -> Error {
    Error::Connection(format!("Cannot send to the broker: {err}"))
}

/// Queue a command without waiting. A full queue means the broker is down, so fail at once.
fn publish(mqtt: &AsyncClient, command: &Command) -> Result<()> {
    mqtt.try_publish(&command.topic, QoS::AtLeastOnce, false, command.payload.clone())
        .map_err(publish_error)
}

/// Poll the MQTT event loop. Report the first outcome through `ready`, then reconnect on errors.
async fn run_events(
    mut events: EventLoop,
    mqtt: AsyncClient,
    shared: Arc<Shared>,
    topics: Vec<String>,
    credentials: Option<Credentials>,
    ready: oneshot::Sender<std::result::Result<(), String>>,
) {
    let mut ready = Some(ready);
    loop {
        match events.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                for topic in &topics {
                    if let Err(err) = mqtt.try_subscribe(topic, QoS::AtLeastOnce) {
                        warn!("Cannot subscribe to {topic}: {err}");
                    }
                }
            }
            Ok(Event::Incoming(Packet::SubAck(_))) => {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
            }
            Ok(Event::Incoming(Packet::Publish(message))) => {
                shared.ingest(&message.topic, &message.payload);
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => return,
            Ok(_) => {}
            Err(err) => {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Err(err.to_string()));
                    return;
                }
                warn!("Broker connection lost: {err}");
                // The token lasts one hour, so each reconnect asks for a valid one.
                if let Some(credentials) = &credentials {
                    match (credentials.token)().await {
                        Ok(token) => {
                            events.mqtt_options.set_credentials(&credentials.username, token);
                        }
                        Err(err) => warn!("Cannot renew the access token: {err}"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// A connection to a broker. It tracks the reports of all devices.
pub struct QuboClient {
    mqtt: AsyncClient,
    shared: Arc<Shared>,
    path: ControlPath,
    address: String,
    events: Mutex<Option<JoinHandle<()>>>,
    refresh: Mutex<Option<JoinHandle<()>>>,
}

impl QuboClient {
    /// Connect to the broker and subscribe to the device reports.
    ///
    /// Fail if the broker is not reachable or does not accept the connection within `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the token provider fails, and [`Error::Connection`] if the broker cannot be reached or does not accept the connection in time.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use qubo_bridge::client::{ClientConfig, QuboClient};
    /// # use std::time::Duration;
    /// # async fn demo() -> qubo_bridge::error::Result<()> {
    /// let config = ClientConfig::local("127.0.0.1", 1883, Vec::new(), None);
    /// let client = QuboClient::connect(config, Duration::from_secs(5)).await?;
    /// println!("{} devices", client.devices().len());
    /// client.stop().await;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect(config: ClientConfig, timeout: Duration) -> Result<Self> {
        let client_id = format!("qubo-bridge-{:08x}", rand::random::<u32>());
        let mut options = MqttOptions::new(client_id, &config.host, config.port);
        options.set_keep_alive(Duration::from_secs(30));
        if config.tls {
            options.set_transport(Transport::tls_with_default_config());
        }
        if let Some(credentials) = &config.credentials {
            let token = (credentials.token)().await?;
            options.set_credentials(&credentials.username, token);
        }
        let (mqtt, events) = AsyncClient::new(options, 64);
        let client = Self::from_parts(&config, mqtt.clone());
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(run_events(
            events,
            mqtt,
            Arc::clone(&client.shared),
            config.topics,
            config.credentials,
            ready_tx,
        ));
        client.set_events(task);
        match tokio::time::timeout(timeout, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(client),
            Ok(Ok(Err(_))) => {
                client.stop().await;
                Err(Error::Connection(format!("Cannot reach the broker at {}.", client.address)))
            }
            Ok(Err(_)) | Err(_) => {
                client.stop().await;
                Err(Error::Connection(format!(
                    "The broker at {} did not accept us.",
                    client.address
                )))
            }
        }
    }

    fn from_parts(config: &ClientConfig, mqtt: AsyncClient) -> Self {
        Self {
            mqtt,
            shared: Arc::new(Shared::new(config.devices.clone(), config.cache.clone())),
            path: config.path,
            address: format!("{}:{}", config.host, config.port),
            events: Mutex::new(None),
            refresh: Mutex::new(None),
        }
    }

    fn set_events(&self, task: JoinHandle<()>) {
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
    }

    /// The control path of this client.
    pub const fn path(&self) -> ControlPath {
        self.path
    }

    /// Disconnect and save the state cache.
    pub async fn stop(&self) {
        let _ = self.mqtt.disconnect().await;
        let events = self.events.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(mut task) = events {
            // The event loop ends when it has sent the disconnect packet.
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                task.abort();
            }
        }
        let refresh = self.refresh.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(task) = refresh {
            task.abort();
        }
        self.shared.save_cache();
    }

    /// Send refresh commands on a schedule until `stop`. For long-lived processes.
    pub fn start_auto_refresh(&self) {
        let mut slot = self.refresh.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let (mqtt, shared) = (self.mqtt.clone(), Arc::clone(&self.shared));
        *slot = Some(tokio::spawn(async move {
            let origin = Instant::now();
            let mut last_save = Duration::ZERO;
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let now = origin.elapsed();
                for command in shared.due(now) {
                    if let Err(err) = publish(&mqtt, &command) {
                        warn!("Refresh {} failed: {err}", command.topic);
                    }
                }
                if shared.is_dirty() && now.saturating_sub(last_save) >= CACHE_SAVE_INTERVAL {
                    shared.save_cache();
                    last_save = now;
                }
            }
        }));
    }

    /// Set values from another source, such as the cloud shadow. They count as current,
    /// not cached, but they do not confirm a command.
    pub fn seed(&self, device: &Device, service: &str, values: Map<String, Value>) {
        {
            let mut state = self.shared.lock();
            state
                .values
                .entry(device.device_uuid.clone())
                .or_default()
                .entry(service.to_owned())
                .or_default()
                .extend(values);
            state.updated.entry(key(&device.device_uuid, service)).or_insert(0);
        }
        self.shared.notify();
    }

    /// The known devices.
    pub fn devices(&self) -> Vec<Device> {
        self.shared.lock().devices.clone()
    }

    /// The values that a device reported for one service.
    pub fn reported(&self, device: &Device, service: Service) -> Map<String, Value> {
        self.shared
            .lock()
            .values
            .get(&device.device_uuid)
            .and_then(|services| services.get(service.as_str()))
            .cloned()
            .unwrap_or_default()
    }

    /// Wait until the state fulfils a condition. Return false on timeout.
    async fn wait_until(&self, condition: impl Fn(&State) -> bool, timeout: Duration) -> bool {
        // Subscribe first, so that no change between the check and the wait is lost.
        let mut changes = self.shared.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition(&self.shared.lock()) {
                return true;
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                _ => return condition(&self.shared.lock()),
            }
        }
    }

    fn all_fresh(state: &State, commands: &[Command], since: u64) -> bool {
        commands.iter().all(|c| {
            let slot = key(&c.device_uuid, c.reply.as_str());
            state.updated.get(&slot).is_some_and(|tick| *tick >= since)
        })
    }

    /// Publish a command. With a timeout, wait until the device reports its reply service.
    ///
    /// Return false if no reply arrived in time, or if the timeout is zero. The command can
    /// still have worked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connection`] if the command cannot be queued for the broker.
    pub async fn send(&self, command: &Command, timeout: Duration) -> Result<bool> {
        let since = self.shared.tick();
        publish(&self.mqtt, command)?;
        if timeout.is_zero() {
            // Nothing was awaited, so nothing is confirmed.
            return Ok(false);
        }
        let commands = std::slice::from_ref(command);
        Ok(self.wait_until(|s| Self::all_fresh(s, commands, since), timeout).await)
    }

    /// Ask every device for PM2.5, filter and energy readings and wait for the replies.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connection`] if a command cannot be queued for the broker.
    pub async fn refresh(&self, timeout: Duration) -> Result<bool> {
        let since = self.shared.tick();
        let commands: Vec<Command> =
            self.devices().iter().flat_map(protocol::refresh_commands).collect();
        for command in &commands {
            publish(&self.mqtt, command)?;
        }
        Ok(self.wait_until(|s| Self::all_fresh(s, &commands, since), timeout).await)
    }

    /// Switch a socket. Refuse a switch-off above [`POWER_LIMIT_W`] unless `force` is true.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Interlock`] if a switch-off is refused, and [`Error::Connection`] if a command cannot be queued for the broker.
    pub async fn switch_socket(
        &self,
        device: &Device,
        on: bool,
        force: bool,
        timeout: Duration,
    ) -> Result<bool> {
        if !on && !force {
            let fresh = self.send(&protocol::metering_refresh(device, 60), timeout).await?;
            let watts = fresh
                .then(|| protocol::metering(&self.reported(device, Service::Metering)).watts)
                .flatten();
            match watts {
                None => {
                    return Err(Error::Interlock(
                        "No fresh power reading from the socket. Use force to switch off anyway."
                            .to_owned(),
                    ));
                }
                Some(watts) if watts > POWER_LIMIT_W => {
                    return Err(Error::Interlock(format!(
                        "The socket load is {watts:?} W, above the {POWER_LIMIT_W:?} W limit. \
                         Use force to switch off anyway."
                    )));
                }
                Some(_) => {}
            }
        }
        self.send(&protocol::power(device, on), timeout).await
    }

    /// The latest state of every device.
    ///
    /// `null` means not reported yet. `cached` lists the fields that come from an earlier
    /// session and have no live report yet. These values can be old.
    pub fn telemetry(&self) -> Vec<Map<String, Value>> {
        let state = self.shared.lock();
        let no_values = HashMap::new();
        state
            .devices
            .iter()
            .map(|device| {
                let values = state.values.get(&device.device_uuid).unwrap_or(&no_values);
                let reported = |service: Service| values.get(service.as_str());
                let mut row = Map::new();
                row.insert("name".into(), device.label().into());
                row.insert("path".into(), self.path.as_str().into());
                row.insert("kind".into(), device.kind.name().into());
                row.insert("mac".into(), device.mac.clone().into());
                row.insert("online".into(), device.online.into());
                let power = reported(Service::Switch).and_then(|v| v.get("power")).cloned();
                row.insert("power".into(), power.unwrap_or(Value::Null));
                let extra = match device.kind {
                    DeviceKind::Purifier => serde_json::to_value(protocol::purifier_state(values)),
                    DeviceKind::Socket => serde_json::to_value(protocol::metering(
                        &reported(Service::Metering).cloned().unwrap_or_default(),
                    )),
                };
                if let Ok(Value::Object(extra)) = extra {
                    row.extend(extra);
                }
                let mut cached: Vec<&str> = values
                    .keys()
                    .filter(|service| {
                        !state.updated.contains_key(&key(&device.device_uuid, service))
                    })
                    .flat_map(|service| fields(service).iter().copied())
                    .collect();
                cached.sort_unstable();
                row.insert("cached".into(), cached.into());
                row
            })
            .collect()
    }

    /// Save the state cache now.
    pub fn save_cache(&self) {
        self.shared.save_cache();
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use rumqttc::Request;
    use serde_json::json;

    use super::*;

    pub fn device(kind: DeviceKind, uuid: &str) -> Device {
        Device {
            kind,
            mac: "0A:0B:0C:0D:0E:01".to_owned(),
            device_uuid: uuid.to_owned(),
            entity_uuid: "ent-s".to_owned(),
            unit_uuid: "unit-1".to_owned(),
            user_uuid: "user-1".to_owned(),
            online: true,
            name: "desk".to_owned(),
        }
    }

    pub fn socket() -> Device {
        device(DeviceKind::Socket, "dev-s")
    }

    pub fn message(service: &str, values: &Value) -> (String, Vec<u8>) {
        let payload = json!({ "devices": { "services": { service: { "events": { "stateChanged": values } } } } });
        (format!("/monitor/unit-1/dev-s/{service}"), payload.to_string().into_bytes())
    }

    /// What a device answers: the command service, then the report service and its values.
    pub type Reply = (&'static str, (&'static str, Value));
    pub type Published = Arc<Mutex<Vec<(String, Value)>>>;

    /// A client without a network. A task answers each publish like a device would.
    pub fn offline(
        devices: Vec<Device>,
        cache: Option<PathBuf>,
        replies: Vec<Reply>,
    ) -> (QuboClient, Published) {
        let (tx, rx) = flume::bounded::<Request>(32);
        let config = ClientConfig::local("127.0.0.1", 1883, devices, cache);
        let client = QuboClient::from_parts(&config, AsyncClient::from_senders(tx));
        let published: Published = Arc::default();
        let (shared, log) = (Arc::clone(&client.shared), Arc::clone(&published));
        tokio::spawn(async move {
            while let Ok(request) = rx.recv_async().await {
                let Request::Publish(publish) = request else { continue };
                let service = publish.topic.rsplit('/').next().unwrap_or_default().to_owned();
                let body: Value = serde_json::from_slice(&publish.payload).unwrap_or_default();
                log.lock().unwrap().push((service.clone(), body));
                if let Some((_, (reply, values))) = replies.iter().find(|(s, _)| *s == service) {
                    let (topic, payload) = message(reply, values);
                    shared.ingest(&topic, &payload);
                }
            }
        });
        (client, published)
    }

    pub fn meter(power: &str) -> Reply {
        (
            "meteringRefresh",
            (
                "plugMetering",
                json!({ "power": power, "voltage": "230", "current": "180", "consumption": "1.5" }),
            ),
        )
    }

    pub fn switch_reply() -> Reply {
        ("lcSwitchControl", ("lcSwitchControl", json!({ "power": "off" })))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::*;
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn sent_services(published: &Published) -> Vec<String> {
        published.lock().unwrap().iter().map(|(s, _)| s.clone()).collect()
    }

    fn ingest_power(client: &QuboClient, power: &str) {
        let (topic, payload) = message("lcSwitchControl", &json!({ "power": power }));
        client.shared.ingest(&topic, &payload);
    }

    const OFFLINE_BEAT: &str = r#"{"devices" : {"deviceUUID" : "dev-s","entityUUID" : "ent-s","unitUUID" : "unit-1","userUUID" : "user-1","srcDeviceId" : "HSP_0A:0B:0C:0D:0E:01","operationState" : "offline","msgSequenceId" : 0001}}"#;

    mod ingest {
        use super::*;

        #[tokio::test]
        async fn keeps_the_user_name_when_a_heartbeat_arrives() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            client.shared.ingest("/monitor/unit-1/dev-s/heartbeat", OFFLINE_BEAT.as_bytes());
            assert_eq!(client.devices()[0].name, "desk");
        }

        #[tokio::test]
        async fn marks_the_device_offline_for_the_last_will_heartbeat() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            client.shared.ingest("/monitor/unit-1/dev-s/heartbeat", OFFLINE_BEAT.as_bytes());
            assert!(!client.devices()[0].online);
        }

        #[tokio::test]
        async fn adds_an_unknown_device_from_its_heartbeat() {
            let (client, _) = offline(vec![], None, vec![]);
            client.shared.ingest("/monitor/unit-1/dev-s/heartbeat", OFFLINE_BEAT.as_bytes());
            assert_eq!(client.devices().len(), 1);
        }

        #[tokio::test]
        async fn ignores_a_topic_that_is_not_a_monitor_topic() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            client.shared.ingest("/control/unit-1/dev-s/lcSwitchControl", b"{}");
            assert_eq!(client.telemetry()[0]["power"], Value::Null);
        }
    }

    mod telemetry {
        use super::*;

        async fn row_after_refresh() -> Map<String, Value> {
            let (client, _) = offline(vec![socket()], None, vec![meter("40")]);
            client.refresh(SECOND).await.unwrap();
            client.telemetry().remove(0)
        }

        #[tokio::test]
        async fn shows_the_device_label_as_the_name() {
            assert_eq!(row_after_refresh().await["name"], "desk");
        }

        #[tokio::test]
        async fn reports_the_power_in_watts() {
            assert_eq!(row_after_refresh().await["watts"], 40.0);
        }

        #[tokio::test]
        async fn converts_the_current_to_amperes() {
            assert_eq!(row_after_refresh().await["amps"], 0.18);
        }

        #[tokio::test]
        async fn reports_the_energy_in_kilowatt_hours() {
            assert_eq!(row_after_refresh().await["kwh"], 1.5);
        }

        #[tokio::test]
        async fn shows_null_for_a_value_that_is_not_reported() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            assert_eq!(client.telemetry()[0]["power"], Value::Null);
        }
    }

    mod switch_socket {
        use super::*;

        #[tokio::test]
        async fn refuses_a_switch_off_above_the_load_limit() {
            let (client, _) = offline(vec![socket()], None, vec![meter("40"), switch_reply()]);
            let err = client.switch_socket(&socket(), false, false, SECOND).await.unwrap_err();
            assert!(matches!(err, Error::Interlock(ref m) if m.contains("40.0 W")), "{err}");
        }

        #[tokio::test]
        async fn sends_only_the_metering_request_when_it_refuses() {
            let (client, published) =
                offline(vec![socket()], None, vec![meter("40"), switch_reply()]);
            let _ = client.switch_socket(&socket(), false, false, SECOND).await;
            assert_eq!(sent_services(&published), ["meteringRefresh"]);
        }

        #[tokio::test]
        async fn refuses_a_switch_off_without_a_power_reading() {
            let (client, _) = offline(vec![socket()], None, vec![switch_reply()]);
            let wait = Duration::from_millis(200);
            let err = client.switch_socket(&socket(), false, false, wait).await.unwrap_err();
            assert!(
                matches!(err, Error::Interlock(ref m) if m.starts_with("No fresh power reading")),
                "{err}"
            );
        }

        #[tokio::test]
        async fn confirms_a_switch_off_at_low_load() {
            let (client, _) = offline(vec![socket()], None, vec![meter("3.5"), switch_reply()]);
            assert!(client.switch_socket(&socket(), false, false, SECOND).await.unwrap());
        }

        #[tokio::test]
        async fn sends_the_power_off_payload_at_low_load() {
            let (client, published) =
                offline(vec![socket()], None, vec![meter("3.5"), switch_reply()]);
            client.switch_socket(&socket(), false, false, SECOND).await.unwrap();
            let last = published.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                last.1["command"]["devices"]["services"],
                json!({ "lcSwitchControl": { "attributes": { "power": "off" }, "instanceId": 0 } })
            );
        }

        #[tokio::test]
        async fn skips_the_power_reading_with_force() {
            let (client, published) =
                offline(vec![socket()], None, vec![meter("40"), switch_reply()]);
            client.switch_socket(&socket(), false, true, SECOND).await.unwrap();
            assert_eq!(sent_services(&published), ["lcSwitchControl"]);
        }

        #[tokio::test]
        async fn skips_the_power_reading_for_a_switch_on() {
            let (client, published) = offline(vec![socket()], None, vec![switch_reply()]);
            client.switch_socket(&socket(), true, false, SECOND).await.unwrap();
            assert_eq!(sent_services(&published), ["lcSwitchControl"]);
        }
    }

    mod due {
        use super::*;

        const PURIFIER_UUID: &str = "dev-p";

        fn due_at(client: &QuboClient, secs: u64) -> Vec<(String, Service)> {
            let mut found: Vec<_> = client
                .shared
                .due(Duration::from_secs(secs))
                .into_iter()
                .map(|c| (c.device_uuid, c.reply))
                .collect();
            found.sort();
            found
        }

        fn client() -> QuboClient {
            let purifier = device(DeviceKind::Purifier, PURIFIER_UUID);
            offline(vec![socket(), purifier], None, vec![]).0
        }

        fn purifier(service: Service) -> (String, Service) {
            (PURIFIER_UUID.to_owned(), service)
        }

        #[tokio::test]
        async fn asks_every_service_at_the_start() {
            let found = due_at(&client(), 0);
            let expected = [
                purifier(Service::Aqi),
                purifier(Service::Filter),
                ("dev-s".to_owned(), Service::Metering),
            ];
            assert_eq!(found, expected);
        }

        #[tokio::test]
        async fn asks_nothing_before_the_first_interval_ends() {
            let client = client();
            due_at(&client, 0);
            assert_eq!(due_at(&client, 29), []);
        }

        #[tokio::test]
        async fn asks_for_air_quality_every_30_seconds() {
            let client = client();
            due_at(&client, 0);
            assert_eq!(due_at(&client, 30), [purifier(Service::Aqi)]);
        }

        #[tokio::test]
        async fn asks_for_energy_every_60_seconds() {
            let client = client();
            due_at(&client, 0);
            assert!(due_at(&client, 60).contains(&("dev-s".to_owned(), Service::Metering)));
        }

        #[tokio::test]
        async fn asks_for_the_filter_life_every_hour() {
            let client = client();
            due_at(&client, 0);
            assert!(due_at(&client, 3600).contains(&purifier(Service::Filter)));
        }
    }

    mod state_cache {
        use super::*;

        fn cache_in(dir: &tempfile::TempDir) -> PathBuf {
            dir.path().join("state.json")
        }

        fn saved_client(dir: &tempfile::TempDir) {
            let (client, _) = offline(vec![socket()], Some(cache_in(dir)), vec![]);
            ingest_power(&client, "on");
            client.save_cache();
        }

        #[tokio::test]
        async fn restores_the_saved_value_in_the_next_session() {
            let dir = tempfile::tempdir().unwrap();
            saved_client(&dir);
            let (client, _) = offline(vec![socket()], Some(cache_in(&dir)), vec![]);
            assert_eq!(client.telemetry()[0]["power"], "on");
        }

        #[tokio::test]
        async fn marks_a_restored_value_as_cached() {
            let dir = tempfile::tempdir().unwrap();
            saved_client(&dir);
            let (client, _) = offline(vec![socket()], Some(cache_in(&dir)), vec![]);
            assert_eq!(client.telemetry()[0]["cached"], json!(["power"]));
        }

        #[tokio::test]
        async fn replaces_a_restored_value_with_a_live_report() {
            let dir = tempfile::tempdir().unwrap();
            saved_client(&dir);
            let (client, _) = offline(vec![socket()], Some(cache_in(&dir)), vec![]);
            ingest_power(&client, "off");
            assert_eq!(client.telemetry()[0]["power"], "off");
        }

        #[tokio::test]
        async fn clears_the_cached_mark_after_a_live_report() {
            let dir = tempfile::tempdir().unwrap();
            saved_client(&dir);
            let (client, _) = offline(vec![socket()], Some(cache_in(&dir)), vec![]);
            ingest_power(&client, "off");
            assert_eq!(client.telemetry()[0]["cached"], json!([]));
        }

        #[tokio::test]
        async fn ignores_a_bad_cache_file() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(cache_in(&dir), "{not json").unwrap();
            let (client, _) = offline(vec![socket()], Some(cache_in(&dir)), vec![]);
            assert_eq!(client.telemetry()[0]["power"], Value::Null);
        }
    }

    mod seed {
        use super::*;

        fn seeded() -> QuboClient {
            let (client, _) = offline(vec![socket()], None, vec![]);
            let values = json!({ "power": "on" }).as_object().cloned().unwrap();
            client.seed(&socket(), "lcSwitchControl", values);
            client
        }

        #[tokio::test]
        async fn counts_the_value_as_current_not_cached() {
            assert_eq!(seeded().telemetry()[0]["cached"], json!([]));
        }

        #[tokio::test]
        async fn does_not_confirm_a_command() {
            let sent =
                seeded().send(&protocol::power(&socket(), true), Duration::from_millis(100)).await;
            assert!(!sent.unwrap());
        }
    }

    mod send {
        use super::*;

        #[tokio::test]
        async fn returns_false_without_waiting_when_the_timeout_is_zero() {
            let (client, _) = offline(vec![socket()], None, vec![switch_reply()]);
            let sent = client.send(&protocol::power(&socket(), true), Duration::ZERO).await;
            assert!(!sent.unwrap());
        }

        #[tokio::test]
        async fn returns_true_when_the_device_reports_the_reply_service() {
            let (client, _) = offline(vec![socket()], None, vec![switch_reply()]);
            assert!(client.send(&protocol::power(&socket(), true), SECOND).await.unwrap());
        }

        #[tokio::test]
        async fn fails_at_once_when_the_request_queue_is_full() {
            let (tx, _rx) = flume::bounded::<rumqttc::Request>(1);
            let config = ClientConfig::local("127.0.0.1", 1883, vec![socket()], None);
            let client = QuboClient::from_parts(&config, AsyncClient::from_senders(tx));
            let command = protocol::power(&socket(), true);
            client.send(&command, Duration::ZERO).await.unwrap();
            let second = client.send(&command, Duration::ZERO).await;
            assert!(matches!(second, Err(Error::Connection(_))));
        }
    }
}
