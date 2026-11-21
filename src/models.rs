//! Device records, the configuration file and the file locations.

use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Device family: the `srcDeviceId` prefix (local) or the model number prefix (cloud).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    /// Air purifier.
    #[serde(rename = "HPH")]
    Purifier,
    /// Smart socket.
    #[serde(rename = "HSP")]
    Socket,
}

impl DeviceKind {
    /// All device families.
    pub const ALL: [Self; 2] = [Self::Purifier, Self::Socket];

    /// The prefix in `srcDeviceId` and in the model number.
    ///
    /// # Examples
    ///
    /// ```
    /// use qubo_bridge::models::DeviceKind;
    ///
    /// assert_eq!(DeviceKind::Purifier.prefix(), "HPH");
    /// ```
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Purifier => "HPH",
            Self::Socket => "HSP",
        }
    }

    /// The lower-case name that the output rows show.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Purifier => "purifier",
            Self::Socket => "socket",
        }
    }

    /// Find the family for a prefix such as `HPH`.
    ///
    /// # Examples
    ///
    /// ```
    /// use qubo_bridge::models::DeviceKind;
    ///
    /// assert_eq!(DeviceKind::from_prefix("HSP"), Some(DeviceKind::Socket));
    /// assert_eq!(DeviceKind::from_prefix("HXX"), None);
    /// ```
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.prefix() == prefix)
    }
}

/// Control path. `auto`: local if `qubo serve` runs on this host, else cloud.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum Mode {
    /// Local if the broker answers, else cloud.
    #[default]
    Auto,
    /// The QUBO cloud broker.
    Cloud,
    /// The broker of `qubo serve`.
    Local,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Cloud => "cloud",
            Self::Local => "local",
        })
    }
}

const fn default_true() -> bool {
    true
}

/// One QUBO device. Commands need all four UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Device family.
    pub kind: DeviceKind,
    /// MAC address.
    pub mac: String,
    /// UUID of the device.
    pub device_uuid: String,
    /// UUID of the entity (room) of the device.
    pub entity_uuid: String,
    /// UUID of the home unit.
    pub unit_uuid: String,
    /// UUID of the user account.
    pub user_uuid: String,
    /// False after the last-will message of the device.
    #[serde(default = "default_true")]
    pub online: bool,
    /// Name that the user gave.
    #[serde(default)]
    pub name: String,
}

impl Device {
    /// The name for output: the user name, or the family and the MAC address.
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
    ///
    /// assert_eq!(device.label(), "purifier 0A:0B:0C:0D:0E:0F");
    /// ```
    #[must_use]
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("{} {}", self.kind.name(), self.mac)
        } else {
            self.name.clone()
        }
    }
}

/// The locations of the files of `qubo`.
#[derive(Debug, Clone)]
pub struct Paths {
    config: PathBuf,
}

impl Paths {
    /// Use `QUBO_CONFIG_PATH`, or `~/.config/qubo/config.json`.
    pub fn from_env() -> Self {
        let config = env::var_os("QUBO_CONFIG_PATH").filter(|value| !value.is_empty()).map_or_else(
            || env::home_dir().unwrap_or_default().join(".config/qubo/config.json"),
            PathBuf::from,
        );
        Self { config }
    }

    /// Use a given configuration file. The other files are next to it.
    ///
    /// # Examples
    ///
    /// ```
    /// use qubo_bridge::models::Paths;
    ///
    /// let paths = Paths::new("/home/u/.config/qubo/config.json");
    /// assert!(paths.session().ends_with("session.json"));
    /// ```
    pub fn new(config: impl Into<PathBuf>) -> Self {
        Self { config: config.into() }
    }

    /// The configuration file.
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// The state cache. It sits next to the configuration file.
    #[must_use]
    pub fn state(&self) -> PathBuf {
        self.config.with_file_name("state.json")
    }

    /// The cloud tokens.
    #[must_use]
    pub fn session(&self) -> PathBuf {
        self.config.with_file_name("session.json")
    }

    /// The certificates of the local broker.
    #[must_use]
    pub fn broker_dir(&self) -> PathBuf {
        self.config.with_file_name("broker")
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write a file in one step, so that a reader never sees half a file.
pub(crate) fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // A unique name per call, so that two writers never share a temporary file.
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let unique = format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_file_name(unique);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = options.open(&tmp).and_then(|mut file| file.write_all(data));
    let moved = written.and_then(|()| fs::rename(&tmp, path));
    if moved.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(moved?)
}

/// Control path, local broker address and the known devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Control path.
    #[serde(default)]
    pub mode: Mode,
    /// Host of the local broker.
    #[serde(default = "default_host")]
    pub host: String,
    /// Port of the local broker.
    #[serde(default = "default_port")]
    pub port: u16,
    /// The known devices.
    #[serde(default)]
    pub devices: Vec<Device>,
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

const fn default_port() -> u16 {
    1883
}

impl Default for Config {
    fn default() -> Self {
        Self { mode: Mode::Auto, host: default_host(), port: default_port(), devices: Vec::new() }
    }
}

impl Config {
    /// Read the file. A missing file gives the default configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connection`] if the HTTP client cannot be built, or [`Error::Io`] and [`Error::Json`] if the saved configuration cannot be read.
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(paths.config()) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// Write the file. Return its path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be written, and [`Error::Json`] if the value cannot be encoded.
    pub fn save(&self, paths: &Paths) -> Result<PathBuf> {
        let text = serde_json::to_string_pretty(self)?;
        write_atomic(paths.config(), text.as_bytes(), false)?;
        Ok(paths.config().to_path_buf())
    }

    /// Add or update devices. Keep the names that the user gave. Return true on change.
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
    /// use qubo_bridge::models::Config;
    ///
    /// let mut config = Config::default();
    /// assert!(config.merge([device.clone()]));
    /// // The same device again is not a change.
    /// assert!(!config.merge([device]));
    /// ```
    pub fn merge(&mut self, found: impl IntoIterator<Item = Device>) -> bool {
        let mut changed = false;
        for mut device in found {
            if let Some(old) = self.devices.iter_mut().find(|d| d.device_uuid == device.device_uuid)
            {
                device.name.clone_from(&old.name);
                if device != *old {
                    *old = device;
                    changed = true;
                }
            } else {
                self.devices.push(device);
                changed = true;
            }
        }
        changed
    }
}

/// Return the one device of this kind that matches a name, MAC address or UUID.
///
/// # Errors
///
/// Returns [`Error::Lookup`] if zero or more than one device matches.
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
/// use qubo_bridge::models::find_device;
///
/// let devices = [device];
/// let found = find_device(&devices, DeviceKind::Purifier, None);
/// assert_eq!(found.map(|d| d.device_uuid.as_str()).ok(), Some("dev-1"));
/// assert!(find_device(&devices, DeviceKind::Socket, None).is_err());
/// ```
pub fn find_device<'a>(
    devices: &'a [Device],
    kind: DeviceKind,
    query: Option<&str>,
) -> Result<&'a Device> {
    let query = query.filter(|q| !q.is_empty());
    let matches: Vec<&Device> = devices
        .iter()
        .filter(|d| {
            d.kind == kind && query.is_none_or(|q| q == d.name || q == d.mac || q == d.device_uuid)
        })
        .collect();
    if let [device] = matches[..] {
        return Ok(device);
    }
    let count = matches.len();
    let what = query.map_or_else(|| "no --device filter".to_owned(), |q| format!("'{q}'"));
    Err(Error::Lookup(format!(
        "{count} {} devices match {what}. Run 'qubo discover' or give --device.",
        kind.name()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(kind: DeviceKind, uuid: &str, name: &str) -> Device {
        Device {
            kind,
            mac: format!("mac-{uuid}"),
            device_uuid: uuid.to_owned(),
            entity_uuid: "e".to_owned(),
            unit_uuid: "u".to_owned(),
            user_uuid: "x".to_owned(),
            online: true,
            name: name.to_owned(),
        }
    }

    fn config_with(devices: impl IntoIterator<Item = Device>) -> Config {
        let mut config = Config::default();
        config.merge(devices);
        config
    }

    mod merge {
        use super::*;

        #[test]
        fn reports_a_change_when_a_device_is_new() {
            let mut config = Config::default();
            assert!(config.merge([device(DeviceKind::Socket, "s1", "desk")]));
        }

        #[test]
        fn reports_no_change_when_the_device_is_identical() {
            let mut config = config_with([device(DeviceKind::Socket, "s1", "desk")]);
            assert!(!config.merge([device(DeviceKind::Socket, "s1", "desk")]));
        }

        #[test]
        fn keeps_the_user_name_when_the_found_device_has_none() {
            let mut config = config_with([device(DeviceKind::Socket, "s1", "desk")]);
            config.merge([device(DeviceKind::Socket, "s1", "")]);
            assert_eq!(config.devices[0].name, "desk");
        }

        #[test]
        fn reports_a_change_when_a_field_differs() {
            let mut config = config_with([device(DeviceKind::Socket, "s1", "desk")]);
            let mut offline = device(DeviceKind::Socket, "s1", "");
            offline.online = false;
            assert!(config.merge([offline]));
        }

        #[test]
        fn stores_the_changed_field() {
            let mut config = config_with([device(DeviceKind::Socket, "s1", "desk")]);
            let mut offline = device(DeviceKind::Socket, "s1", "");
            offline.online = false;
            config.merge([offline]);
            assert!(!config.devices[0].online);
        }
    }

    mod find_device {
        use super::*;

        fn two_purifiers() -> [Device; 2] {
            [device(DeviceKind::Purifier, "p1", "a"), device(DeviceKind::Purifier, "p2", "b")]
        }

        #[test]
        fn returns_the_only_device_of_the_kind() {
            let devices =
                [device(DeviceKind::Purifier, "p1", "a"), device(DeviceKind::Socket, "s1", "")];
            let found = find_device(&devices, DeviceKind::Socket, None);
            assert_eq!(found.map(|d| d.device_uuid.as_str()).ok(), Some("s1"));
        }

        #[test]
        fn returns_a_lookup_error_when_two_devices_match() {
            let devices = two_purifiers();
            let found = find_device(&devices, DeviceKind::Purifier, None);
            assert!(matches!(found, Err(Error::Lookup(_))));
        }

        #[test]
        fn names_the_count_in_the_error_message() {
            let devices = two_purifiers();
            let message =
                find_device(&devices, DeviceKind::Purifier, None).unwrap_err().to_string();
            assert!(
                message.starts_with("2 purifier devices match no --device filter."),
                "{message}"
            );
        }

        #[test]
        fn matches_a_device_by_name() {
            let devices = [device(DeviceKind::Purifier, "p1", "bedroom")];
            assert!(find_device(&devices, DeviceKind::Purifier, Some("bedroom")).is_ok());
        }

        #[test]
        fn matches_a_device_by_mac() {
            let devices = [device(DeviceKind::Purifier, "p1", "bedroom")];
            assert!(find_device(&devices, DeviceKind::Purifier, Some("mac-p1")).is_ok());
        }

        #[test]
        fn matches_a_device_by_uuid() {
            let devices = [device(DeviceKind::Purifier, "p1", "bedroom")];
            assert!(find_device(&devices, DeviceKind::Purifier, Some("p1")).is_ok());
        }
    }

    mod config_json {
        use super::*;

        #[test]
        fn stores_the_device_kind_as_its_prefix() {
            let text =
                serde_json::to_string(&config_with([device(DeviceKind::Purifier, "p1", "")]));
            assert!(text.unwrap().contains(r#""kind":"HPH""#));
        }

        #[test]
        fn uses_snake_case_field_names() {
            let text =
                serde_json::to_string(&config_with([device(DeviceKind::Purifier, "p1", "")]));
            assert!(text.unwrap().contains(r#""device_uuid":"p1""#));
        }

        #[test]
        fn reads_a_missing_port_as_the_default() {
            let config: Config = serde_json::from_str("{}").unwrap();
            assert_eq!(config.port, 1883);
        }

        #[test]
        fn reads_a_missing_host_as_loopback() {
            let config: Config = serde_json::from_str(r#"{"port": 1884}"#).unwrap();
            assert_eq!(config.host, "127.0.0.1");
        }
    }

    mod write_atomic {
        use super::*;

        fn entries(dir: &Path) -> Vec<String> {
            fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn writes_the_content() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            write_atomic(&path, b"one", false).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"one");
        }

        #[test]
        fn leaves_no_temporary_file_behind() {
            let dir = tempfile::tempdir().unwrap();
            write_atomic(&dir.path().join("state.json"), b"one", false).unwrap();
            assert_eq!(entries(dir.path()), ["state.json"]);
        }

        #[test]
        fn replaces_an_existing_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            write_atomic(&path, b"one", false).unwrap();
            write_atomic(&path, b"two", false).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"two");
        }

        #[test]
        fn keeps_whole_content_when_many_threads_write_at_once() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let writers: Vec<_> = (0..8)
                .map(|n| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        write_atomic(&path, format!("{n}").repeat(5000).as_bytes(), false)
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
            let text = fs::read_to_string(&path).unwrap();
            let one_writer = (0..8).any(|n| text == format!("{n}").repeat(5000));
            assert!(one_writer, "the file mixes the content of several writers");
        }

        #[test]
        fn leaves_no_temporary_file_after_many_threads_write() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let writers: Vec<_> = (0..8)
                .map(|n| {
                    let path = path.clone();
                    std::thread::spawn(move || write_atomic(&path, &[n], false))
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
            assert_eq!(entries(dir.path()), ["state.json"]);
        }
    }

    mod paths {
        use super::*;

        #[test]
        fn puts_the_session_file_next_to_the_config_file() {
            assert_eq!(
                Paths::new("/x/qubo/config.json").session(),
                PathBuf::from("/x/qubo/session.json")
            );
        }

        #[test]
        fn puts_the_broker_directory_next_to_the_config_file() {
            assert_eq!(
                Paths::new("/x/qubo/config.json").broker_dir(),
                PathBuf::from("/x/qubo/broker")
            );
        }
    }
}
