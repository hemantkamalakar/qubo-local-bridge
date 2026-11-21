//! QUBO MQTT protocol: topics, command payloads and message parsing.
//!
//! Devices connect to `mqtt.platform.quboworld.com` on TLS port 8883. Commands go to
//! `/control/<unitUUID>/<deviceUUID>/<service>`. Devices report on
//! `/monitor/<unitUUID>/<deviceUUID>/<service>`. This module does no I/O.
//!
//! # Attribution
//!
//! This module follows the design and the test cases of the MIT-licensed project
//! "qubo-local-control" (dtechterminal/qubo-local-control).
//!
//! Copyright (c) 2024 Ankur Pandey. The MIT license of that project requires its copyright
//! notice and permission notice in all copies or substantial portions of the software. The
//! full text is in the `NOTICE` file.

use std::fmt;
use std::io;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::models::{Device, DeviceKind};

/// Host name of the QUBO cloud broker. The devices check this name in the certificate.
pub const CLOUD_HOST: &str = "mqtt.platform.quboworld.com";
/// Topic filter for all device reports.
pub const MONITOR_ALL: &str = "/monitor/#";

/// Device services. Each service is also the last segment of its topic.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    /// Power switch.
    Switch,
    /// Fan speed.
    FanSpeed,
    /// Fan mode.
    FanMode,
    /// Air quality report.
    Aqi,
    /// Request for an air quality report.
    AqiRefresh,
    /// Filter life.
    Filter,
    /// Socket energy report.
    Metering,
    /// Request for a socket energy report.
    MeteringRefresh,
    /// Heartbeat.
    Heartbeat,
}

impl Service {
    /// The service name in topics and payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Switch => "lcSwitchControl",
            Self::FanSpeed => "fanSpeedControl",
            Self::FanMode => "fanControlMode",
            Self::Aqi => "aqiStatus",
            Self::AqiRefresh => "aqiRefresh",
            Self::Filter => "filterReset",
            Self::Metering => "plugMetering",
            Self::MeteringRefresh => "meteringRefresh",
            Self::Heartbeat => "heartbeat",
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fan mode of a purifier.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum FanMode {
    /// The purifier picks the speed.
    Auto,
    /// The user picks the speed.
    Manual,
}

impl FanMode {
    /// The value in the payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// A command for a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The target device.
    pub device_uuid: String,
    /// The `/control` topic.
    pub topic: String,
    /// The JSON payload.
    pub payload: String,
    /// The monitor service that confirms the command.
    pub reply: Service,
}

/// Return (unit UUID, device UUID, service) for a `/monitor` topic, else `None`.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::parse_topic;
///
/// let topic = "/monitor/unit-1/dev-1/aqiStatus";
/// assert_eq!(parse_topic(topic), Some(("unit-1", "dev-1", "aqiStatus")));
/// assert_eq!(parse_topic("/control/unit-1/dev-1/aqiStatus"), None);
/// ```
#[must_use]
pub fn parse_topic(topic: &str) -> Option<(&str, &str, &str)> {
    match topic.split('/').collect::<Vec<_>>()[..] {
        ["", "monitor", unit, device, service] => Some((unit, device, service)),
        _ => None,
    }
}

// The firmware writes zero-padded integers ("msgSequenceId" : 0001), which JSON rejects.
// Only a number right after an object key is changed, so string values stay as they are.
static PADDED_INT: LazyLock<Regex> = LazyLock::new(|| {
    // The pattern is a constant. It always compiles.
    #[expect(clippy::expect_used, reason = "constant pattern")]
    Regex::new(r#"("\s*:\s*)0+(\d)"#).expect("valid regex")
});

fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Decode a device message. Return `None` if it is not a JSON object.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::decode;
///
/// // The firmware writes zero-padded numbers. Plain JSON rejects them.
/// let message = decode(r#"{"msgSequenceId" : 0001}"#).unwrap_or_default();
/// assert_eq!(message["msgSequenceId"], 1);
/// assert!(decode("[1]").is_none());
/// ```
pub fn decode(payload: impl AsRef<[u8]>) -> Option<Map<String, Value>> {
    let text = String::from_utf8_lossy(payload.as_ref());
    // A valid object ends the search. An invalid one can still pass after the padding fix.
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        return value.as_object().cloned();
    }
    parse_object(&PADDED_INT.replace_all(&text, "$1$2"))
}

fn get_object<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    map.get(key).and_then(Value::as_object)
}

fn non_empty_str<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Return the device that sent a heartbeat.
///
/// Return `None` for the QUBO phone app (its `srcDeviceId` is a bare UUID) and for
/// unknown device families.
///
/// # Examples
///
/// ```
/// use qubo_bridge::models::DeviceKind;
/// use qubo_bridge::protocol::parse_heartbeat;
///
/// let beat = r#"{"devices" : {"deviceUUID" : "d","entityUUID" : "e","unitUUID" : "u","userUUID" : "x","srcDeviceId" : "HSP_0A:0B:0C:0D:0E:01"}}"#;
/// let device = parse_heartbeat(beat).expect("a socket heartbeat");
/// assert_eq!(device.kind, DeviceKind::Socket);
/// assert!(device.online);
/// ```
pub fn parse_heartbeat(payload: impl AsRef<[u8]>) -> Option<Device> {
    let message = decode(payload)?;
    let body = get_object(&message, "devices")?;
    let source = body.get("srcDeviceId").and_then(Value::as_str).unwrap_or_default();
    let (prefix, mac) = source.split_once('_').unwrap_or((source, ""));
    let kind = DeviceKind::from_prefix(prefix)?;
    let id = |key: &str| non_empty_str(body, key).map(str::to_owned);
    if mac.is_empty() {
        return None;
    }
    Some(Device {
        kind,
        mac: mac.to_owned(),
        device_uuid: id("deviceUUID")?,
        entity_uuid: id("entityUUID")?,
        unit_uuid: id("unitUUID")?,
        user_uuid: id("userUUID")?,
        online: body.get("operationState").and_then(Value::as_str) != Some("offline"),
        name: String::new(),
    })
}

/// Return the values in a monitor message for one service.
///
/// Most messages use `services.<service>.events.stateChanged`. Some replies use
/// `services.<service>.attributes`. String values lose their outer spaces.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::reported_values;
///
/// let report = r#"{"devices": {"services": {"aqiStatus": {"events": {"stateChanged": {"PM25": " 12"}}}}}}"#;
/// let values = reported_values(report, "aqiStatus");
/// assert_eq!(values["PM25"], "12");
/// ```
pub fn reported_values(payload: impl AsRef<[u8]>, service: &str) -> Map<String, Value> {
    let found = decode(payload).and_then(|message| {
        let devices = get_object(&message, "devices")?;
        let entry = get_object(get_object(devices, "services")?, service)?;
        let changed = get_object(entry, "events").and_then(|e| get_object(e, "stateChanged"));
        changed
            .filter(|values| !values.is_empty())
            .or_else(|| get_object(entry, "attributes"))
            .cloned()
    });
    found
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value)| match value {
            Value::String(text) => (key, Value::String(text.trim().to_owned())),
            other => (key, other),
        })
        .collect()
}

struct SpacedFormatter;

/// Put one space after each comma and colon. The firmware parser is not known to accept
/// the compact form, so commands use this layout.
impl serde_json::ser::Formatter for SpacedFormatter {
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
}

/// Write JSON with a space after each comma and colon.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::dumps;
/// use serde_json::json;
///
/// assert_eq!(dumps(&json!({"a": [1, 2]})), r#"{"a": [1, 2]}"#);
/// ```
#[must_use]
pub fn dumps(value: &Value) -> String {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, SpacedFormatter);
    // A Value always serializes, and the buffer is UTF-8.
    if Serialize::serialize(value, &mut serializer).is_err() {
        return String::new();
    }
    String::from_utf8(out).unwrap_or_default()
}

fn command(device: &Device, service: Service, body: Value, reply: Service) -> Command {
    let mut devices = Map::new();
    devices.insert("deviceUUID".to_owned(), json!(device.device_uuid));
    if let Value::Object(extra) = body {
        devices.extend(extra);
    }
    Command {
        device_uuid: device.device_uuid.clone(),
        topic: format!("/control/{}/{}/{service}", device.unit_uuid, device.device_uuid),
        payload: dumps(&json!({ "command": { "devices": devices } })),
        reply,
    }
}

fn set(device: &Device, service: Service, attributes: &Value) -> Command {
    let body = json!({
        "entityUUID": device.entity_uuid,
        "services": { service.as_str(): { "attributes": attributes, "instanceId": 0 } },
    });
    command(device, service, body, service)
}

fn request(device: &Device, service: Service, name: &str, reply: Service) -> Command {
    let body = json!({
        "handleName": device.user_uuid,
        "services": {
            service.as_str(): { "commands": { name: { "instanceId": 0, "parameters": {} } } }
        },
    });
    command(device, service, body, reply)
}

/// Switch a device on or off.
///
/// # Examples
///
/// ```
/// # use qubo_bridge::models::{Device, DeviceKind};
/// # let device = Device {
/// #     kind: DeviceKind::Purifier,
/// #     mac: "0A:0B:0C:0D:0E:0F".into(),
/// #     device_uuid: "dev-1".into(),
/// #     entity_uuid: "ent-1".into(),
/// #     unit_uuid: "unit-1".into(),
/// #     user_uuid: "user-1".into(),
/// #     online: true,
/// #     name: String::new(),
/// # };
/// use qubo_bridge::protocol::{Service, power};
///
/// let command = power(&device, true);
/// assert_eq!(command.topic, "/control/unit-1/dev-1/lcSwitchControl");
/// assert_eq!(command.reply, Service::Switch);
/// ```
#[must_use]
pub fn power(device: &Device, on: bool) -> Command {
    set(device, Service::Switch, &json!({ "power": if on { "on" } else { "off" } }))
}

/// Set the fan speed of a purifier: 1, 2 or 3.
///
/// # Errors
///
/// Returns [`Error::Value`] if `speed` is not 1, 2 or 3.
///
/// # Examples
///
/// ```
/// # use qubo_bridge::models::{Device, DeviceKind};
/// # let device = Device {
/// #     kind: DeviceKind::Purifier,
/// #     mac: "0A:0B:0C:0D:0E:0F".into(),
/// #     device_uuid: "dev-1".into(),
/// #     entity_uuid: "ent-1".into(),
/// #     unit_uuid: "unit-1".into(),
/// #     user_uuid: "user-1".into(),
/// #     online: true,
/// #     name: String::new(),
/// # };
/// use qubo_bridge::protocol::fan_speed;
///
/// assert!(fan_speed(&device, 2).is_ok());
/// assert!(fan_speed(&device, 4).is_err());
/// ```
pub fn fan_speed(device: &Device, speed: i64) -> Result<Command> {
    if !(1..=3).contains(&speed) {
        return Err(Error::Value(format!("Fan speed must be 1, 2 or 3, not {speed}.")));
    }
    Ok(set(device, Service::FanSpeed, &json!({ "speed": speed.to_string() })))
}

/// Set the fan mode of a purifier.
///
/// # Examples
///
/// ```
/// # use qubo_bridge::models::{Device, DeviceKind};
/// # let device = Device {
/// #     kind: DeviceKind::Purifier,
/// #     mac: "0A:0B:0C:0D:0E:0F".into(),
/// #     device_uuid: "dev-1".into(),
/// #     entity_uuid: "ent-1".into(),
/// #     unit_uuid: "unit-1".into(),
/// #     user_uuid: "user-1".into(),
/// #     online: true,
/// #     name: String::new(),
/// # };
/// use qubo_bridge::protocol::{FanMode, fan_mode};
///
/// let command = fan_mode(&device, FanMode::Auto);
/// assert!(command.payload.contains(r#""state": "auto""#));
/// ```
#[must_use]
pub fn fan_mode(device: &Device, mode: FanMode) -> Command {
    set(device, Service::FanMode, &json!({ "state": mode.as_str() }))
}

/// Ask a purifier for a PM2.5 report.
#[must_use]
pub fn aqi_refresh(device: &Device) -> Command {
    request(device, Service::AqiRefresh, "refresh", Service::Aqi)
}

/// Ask a purifier for the filter life.
///
/// This is only the status query. The command that resets the filter counter is not known.
#[must_use]
pub fn filter_status(device: &Device) -> Command {
    request(device, Service::Filter, "getCurrentStatus", Service::Filter)
}

/// Ask a socket to report `plugMetering` for the next `seconds` seconds.
#[must_use]
pub fn metering_refresh(device: &Device, seconds: u32) -> Command {
    let body = json!({
        "handleName": device.user_uuid,
        "services": {
            Service::MeteringRefresh.as_str(): {
                "attributes": { "duration": seconds.to_string() },
                "instanceId": 0,
            }
        },
    });
    command(device, Service::MeteringRefresh, body, Service::Metering)
}

/// The commands that ask a device for its sensor values.
#[must_use]
pub fn refresh_commands(device: &Device) -> Vec<Command> {
    match device.kind {
        DeviceKind::Purifier => vec![aqi_refresh(device), filter_status(device)],
        DeviceKind::Socket => vec![metering_refresh(device, 60)],
    }
}

/// Read a number from a report value. A value can be a number or a string.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::number;
/// use serde_json::json;
///
/// let values = json!({"current": "54"});
/// let values = values.as_object().cloned().unwrap_or_default();
/// assert_eq!(number(&values, "current", 0.001), Some(0.054));
/// assert_eq!(number(&values, "missing", 1.0), None);
/// ```
#[must_use]
pub fn number(values: &Map<String, Value>, key: &str, scale: f64) -> Option<f64> {
    let raw = match values.get(key)? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return None,
    };
    let scaled = raw * scale;
    scaled.is_finite().then(|| (scaled * 1000.0).round() / 1000.0)
}

/// Socket energy values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Metering {
    /// Power in watts.
    pub watts: Option<f64>,
    /// Voltage in volts.
    pub volts: Option<f64>,
    /// Current in amperes.
    pub amps: Option<f64>,
    /// Energy in kilowatt hours.
    pub kwh: Option<f64>,
}

/// Convert `plugMetering` values to watts, volts, amperes and kWh. The plug reports mA.
///
/// # Examples
///
/// ```
/// use qubo_bridge::protocol::metering;
/// use serde_json::json;
///
/// // The plug reports the current in mA.
/// let values = json!({"power": "12.5", "current": "54"});
/// let values = values.as_object().cloned().unwrap_or_default();
/// let energy = metering(&values);
/// assert_eq!((energy.watts, energy.amps), (Some(12.5), Some(0.054)));
/// ```
#[must_use]
pub fn metering(values: &Map<String, Value>) -> Metering {
    Metering {
        watts: number(values, "power", 1.0),
        volts: number(values, "voltage", 1.0),
        amps: number(values, "current", 0.001),
        kwh: number(values, "consumption", 1.0),
    }
}

/// Purifier values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurifierState {
    /// Fan speed, as reported.
    pub speed: Option<Value>,
    /// Fan mode, as reported.
    pub mode: Option<Value>,
    /// PM2.5 in micrograms per cubic metre.
    pub pm25: Option<i64>,
    /// Hours of filter life left.
    pub filter_hours: Option<i64>,
}

/// Collect the purifier values from the values of all services.
pub fn purifier_state<S: std::hash::BuildHasher>(
    values: &std::collections::HashMap<String, Map<String, Value>, S>,
) -> PurifierState {
    let service = |service: Service| values.get(service.as_str());
    let empty = Map::new();
    let get = |service_: Service, key: &str| service(service_).and_then(|v| v.get(key)).cloned();
    // "as" truncates toward zero. The value is a small positive number.
    #[expect(clippy::cast_possible_truncation, reason = "truncation is the intent")]
    let whole = |service_: Service, key: &str| {
        number(service(service_).unwrap_or(&empty), key, 1.0).map(|n| n as i64)
    };
    PurifierState {
        speed: get(Service::FanSpeed, "speed"),
        mode: get(Service::FanMode, "state"),
        pm25: whole(Service::Aqi, "PM25"),
        filter_hours: whole(Service::Filter, "timeRemaining"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: &str = "unit-0001";
    const DEV: &str = "dev-0002";
    const ENT: &str = "ent-0003";
    const USER: &str = "user-0004";

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn purifier() -> Device {
        Device {
            kind: DeviceKind::Purifier,
            mac: "0A:0B:0C:0D:0E:0F".to_owned(),
            device_uuid: DEV.to_owned(),
            entity_uuid: ENT.to_owned(),
            unit_uuid: UNIT.to_owned(),
            user_uuid: USER.to_owned(),
            online: true,
            name: String::new(),
        }
    }

    // Firmware style: spaces around colons and bare, sometimes zero-padded, integers.
    fn heartbeat(src: &str, state: &str, seq: &str) -> String {
        format!(
            r#"{{"devices" : {{"action" : 140,"deviceUUID" : "{DEV}","entityUUID" : "{ENT}","msgSequenceId" : {seq},"operationState" : "{state}","srcDeviceId" : "{src}","timestamp" : {seq},"unitUUID" : "{UNIT}","userUUID" : "{USER}"}}}}"#
        )
    }

    fn offline_socket_beat() -> String {
        heartbeat("HSP_0A:0B:0C:0D:0E:01", "offline", "0007")
    }

    fn report(service: &str, values: &Value, attributes: bool) -> String {
        let entry = if attributes {
            json!({ "attributes": values })
        } else {
            json!({ "events": { "stateChanged": values } })
        };
        json!({ "devices": { "deviceUUID": DEV, "services": { service: entry } } }).to_string()
    }

    mod parse_topic {
        use super::*;

        #[test]
        fn returns_the_ids_and_service_for_a_monitor_topic() {
            let topic = format!("/monitor/{UNIT}/{DEV}/aqiStatus");
            assert_eq!(parse_topic(&topic), Some((UNIT, DEV, "aqiStatus")));
        }

        #[test]
        fn returns_none_for_a_control_topic() {
            assert_eq!(parse_topic(&format!("/control/{UNIT}/{DEV}/aqiStatus")), None);
        }

        #[test]
        fn returns_none_for_a_short_topic() {
            assert_eq!(parse_topic("/monitor/x"), None);
        }
    }

    mod decode {
        use super::*;

        fn decoded(text: &str) -> Value {
            decode(text).map_or(Value::Null, Value::Object)
        }

        #[test]
        fn reads_a_zero_padded_number() {
            assert_eq!(decoded(r#"{"n" : 007}"#), json!({ "n": 7 }));
        }

        #[test]
        fn keeps_digits_inside_a_string() {
            assert_eq!(decoded(r#"{"a" : "b:007"}"#), json!({ "a": "b:007" }));
        }

        #[test]
        fn keeps_a_fraction_that_starts_with_zero() {
            assert_eq!(decoded(r#"{"f" : 0.25}"#), json!({ "f": 0.25 }));
        }

        #[test]
        fn reads_a_run_of_zeros_as_one_zero() {
            assert_eq!(decoded(r#"{"n" : 000}"#), json!({ "n": 0 }));
        }

        #[test]
        fn keeps_a_single_zero() {
            assert_eq!(decoded(r#"{"n" : 0}"#), json!({ "n": 0 }));
        }

        #[test]
        fn returns_none_for_an_empty_payload() {
            assert_eq!(decode(""), None);
        }

        #[test]
        fn returns_none_for_text() {
            assert_eq!(decode("x"), None);
        }

        #[test]
        fn returns_none_for_an_array() {
            assert_eq!(decode("[1]"), None);
        }

        #[test]
        fn returns_none_for_a_string() {
            assert_eq!(decode(r#""s""#), None);
        }

        #[test]
        fn returns_none_for_a_truncated_object() {
            assert_eq!(decode("{"), None);
        }
    }

    mod parse_heartbeat {
        use super::*;

        fn device() -> Device {
            parse_heartbeat(offline_socket_beat()).unwrap()
        }

        #[test]
        fn is_needed_because_plain_json_rejects_the_padded_message() {
            assert!(serde_json::from_str::<Value>(&offline_socket_beat()).is_err());
        }

        #[test]
        fn reads_the_device_kind_from_the_source_prefix() {
            assert_eq!(device().kind, DeviceKind::Socket);
        }

        #[test]
        fn reads_the_mac_address() {
            assert_eq!(device().mac, "0A:0B:0C:0D:0E:01");
        }

        #[test]
        fn marks_the_device_offline_for_the_last_will_message() {
            assert!(!device().online);
        }

        #[test]
        fn reads_the_unit_uuid() {
            assert_eq!(device().unit_uuid, UNIT);
        }

        #[test]
        fn reads_the_user_uuid() {
            assert_eq!(device().user_uuid, USER);
        }

        #[test]
        fn marks_the_device_online_by_default() {
            let beat = heartbeat("HPH_0A:0B:0C:0D:0E:02", "online", "1");
            assert!(parse_heartbeat(beat).unwrap().online);
        }

        #[test]
        fn ignores_the_phone_app() {
            let beat = heartbeat("9F1C2B3A-0000-4000-8000-000000000000", "online", "1");
            assert_eq!(parse_heartbeat(beat), None);
        }

        #[test]
        fn ignores_a_prefix_without_a_mac_address() {
            assert_eq!(parse_heartbeat(heartbeat("HPH_", "online", "1")), None);
        }

        #[test]
        fn ignores_an_unknown_device_family() {
            assert_eq!(parse_heartbeat(heartbeat("HXX_01", "online", "1")), None);
        }

        #[test]
        fn ignores_an_empty_source() {
            assert_eq!(parse_heartbeat(heartbeat("", "online", "1")), None);
        }

        #[test]
        fn returns_none_for_invalid_json() {
            assert_eq!(parse_heartbeat("{"), None);
        }
    }

    mod reported_values {
        use super::*;

        #[test]
        fn reads_the_state_changed_values() {
            let aqi = report("aqiStatus", &json!({ "PM25": "12" }), false);
            assert_eq!(Value::Object(reported_values(&aqi, "aqiStatus")), json!({ "PM25": "12" }));
        }

        #[test]
        fn returns_nothing_for_another_service() {
            let aqi = report("aqiStatus", &json!({ "PM25": "12" }), false);
            assert!(reported_values(&aqi, "filterReset").is_empty());
        }

        #[test]
        fn reads_the_attributes_when_there_is_no_state_changed_event() {
            let switch = report("lcSwitchControl", &json!({ "power": "on" }), true);
            assert_eq!(
                Value::Object(reported_values(&switch, "lcSwitchControl")),
                json!({ "power": "on" })
            );
        }

        #[test]
        fn trims_the_outer_spaces_of_string_values() {
            let switch = report("lcSwitchControl", &json!({ "power": " on" }), true);
            assert_eq!(reported_values(&switch, "lcSwitchControl")["power"], "on");
        }

        #[test]
        fn returns_nothing_for_invalid_json() {
            assert!(reported_values("{", "aqiStatus").is_empty());
        }
    }

    mod dumps {
        use super::*;

        #[test]
        fn puts_a_space_after_each_comma_and_colon() {
            assert_eq!(dumps(&json!({ "a": [1, 2], "b": 3 })), r#"{"a": [1, 2], "b": 3}"#);
        }
    }

    mod fan_speed {
        use super::*;

        fn command() -> Command {
            fan_speed(&purifier(), 2).unwrap()
        }

        #[test]
        fn sends_to_the_control_topic_of_the_service() {
            assert_eq!(command().topic, "/control/unit-0001/dev-0002/fanSpeedControl");
        }

        #[test]
        fn expects_the_fan_speed_report_as_the_reply() {
            assert_eq!(command().reply, Service::FanSpeed);
        }

        #[test]
        fn builds_the_exact_payload() {
            assert_eq!(
                command().payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "entityUUID": "ent-0003", "services": {"fanSpeedControl": {"attributes": {"speed": "2"}, "instanceId": 0}}}}}"#
            );
        }

        #[test]
        fn refuses_a_speed_above_3() {
            let message = fan_speed(&purifier(), 4).unwrap_err().to_string();
            assert_eq!(message, "Fan speed must be 1, 2 or 3, not 4.");
        }

        #[test]
        fn refuses_a_speed_of_0() {
            assert!(fan_speed(&purifier(), 0).is_err());
        }
    }

    mod fan_mode {
        use super::*;

        #[test]
        fn builds_the_exact_payload() {
            assert_eq!(
                fan_mode(&purifier(), FanMode::Auto).payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "entityUUID": "ent-0003", "services": {"fanControlMode": {"attributes": {"state": "auto"}, "instanceId": 0}}}}}"#
            );
        }
    }

    mod power {
        use super::*;

        #[test]
        fn builds_the_exact_payload_for_off() {
            assert_eq!(
                power(&purifier(), false).payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "entityUUID": "ent-0003", "services": {"lcSwitchControl": {"attributes": {"power": "off"}, "instanceId": 0}}}}}"#
            );
        }

        #[test]
        fn sends_on_for_true() {
            assert!(power(&purifier(), true).payload.contains(r#""power": "on""#));
        }
    }

    mod aqi_refresh {
        use super::*;

        #[test]
        fn expects_the_aqi_report_as_the_reply() {
            assert_eq!(aqi_refresh(&purifier()).reply, Service::Aqi);
        }

        #[test]
        fn builds_the_exact_payload() {
            assert_eq!(
                aqi_refresh(&purifier()).payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "handleName": "user-0004", "services": {"aqiRefresh": {"commands": {"refresh": {"instanceId": 0, "parameters": {}}}}}}}}"#
            );
        }
    }

    mod filter_status {
        use super::*;

        #[test]
        fn builds_the_exact_payload() {
            assert_eq!(
                filter_status(&purifier()).payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "handleName": "user-0004", "services": {"filterReset": {"commands": {"getCurrentStatus": {"instanceId": 0, "parameters": {}}}}}}}}"#
            );
        }
    }

    mod metering_refresh {
        use super::*;

        #[test]
        fn expects_the_metering_report_as_the_reply() {
            assert_eq!(metering_refresh(&purifier(), 30).reply, Service::Metering);
        }

        #[test]
        fn builds_the_exact_payload() {
            assert_eq!(
                metering_refresh(&purifier(), 30).payload,
                r#"{"command": {"devices": {"deviceUUID": "dev-0002", "handleName": "user-0004", "services": {"meteringRefresh": {"attributes": {"duration": "30"}, "instanceId": 0}}}}}"#
            );
        }
    }

    mod metering {
        use super::*;

        fn values() -> Map<String, Value> {
            obj(json!({ "power": "12.5", "voltage": "231", "current": "54", "consumption": "3.2" }))
        }

        #[test]
        fn reads_the_power_in_watts() {
            assert_eq!(metering(&values()).watts, Some(12.5));
        }

        #[test]
        fn reads_the_voltage_in_volts() {
            assert_eq!(metering(&values()).volts, Some(231.0));
        }

        #[test]
        fn converts_the_current_from_milliamperes_to_amperes() {
            assert_eq!(metering(&values()).amps, Some(0.054));
        }

        #[test]
        fn reads_the_energy_in_kilowatt_hours() {
            assert_eq!(metering(&values()).kwh, Some(3.2));
        }

        #[test]
        fn gives_none_for_a_value_that_is_not_a_number() {
            assert_eq!(metering(&obj(json!({ "power": "n/a" }))).watts, None);
        }
    }

    mod number {
        use super::*;

        #[test]
        fn accepts_a_json_number() {
            assert_eq!(number(&obj(json!({ "a": 7 })), "a", 1.0), Some(7.0));
        }

        #[test]
        fn accepts_a_string_with_outer_spaces() {
            assert_eq!(number(&obj(json!({ "b": " 2.5 " })), "b", 1.0), Some(2.5));
        }

        #[test]
        fn gives_none_for_a_missing_key() {
            assert_eq!(number(&obj(json!({})), "a", 1.0), None);
        }
    }
}
