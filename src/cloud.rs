//! Cloud path: QUBO account login, device list, device shadow and cloud MQTT settings.
//!
//! The REST gateway and the login flow are the ones that the QUBO phone app uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::client::{ClientConfig, Credentials, TokenFuture};
use crate::error::{Error, Result};
use crate::models::{Device, DeviceKind, Paths, write_atomic};
use crate::protocol::Service;

const BASE_URL: &str = "https://srvcapp.platform.quboworld.com:443";
const SERVICE_PROVIDER: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const APP_ID: &str = "com.hero.iot";
const DEVICE_LABEL: &str = "qubo-cli";
/// Renew the access token when it has less than this time left, in seconds.
const RENEW_MARGIN: f64 = 300.0;
/// Services that the devices report only on change. The shadow gives their last value.
const SHADOW_SERVICES: [Service; 3] = [Service::Switch, Service::FanSpeed, Service::FanMode];

/// The tokens of a logged-in account. The session file holds them.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    /// Access token, valid about one hour.
    pub access_token: String,
    /// Refresh token, valid about 180 days.
    pub refresh_token: Option<String>,
    /// UUID of the user account.
    pub user_uuid: String,
    /// Expiry of the access token, in seconds since the Unix epoch.
    pub expires_at: Option<f64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secrets.
        f.debug_struct("Tokens").field("user_uuid", &self.user_uuid).finish_non_exhaustive()
    }
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Read the expiry time from the payload of a JWT. The signature is not checked.
fn jwt_expiry(token: &str) -> Option<f64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_f64()
}

/// Make a short text from an error response of the gateway.
fn error_detail(status: StatusCode, body: &str) -> String {
    let code = status.as_u16();
    if let Ok(Value::Object(data)) = serde_json::from_str::<Value>(body) {
        let text = |keys: &[&str]| {
            keys.iter().find_map(|k| match data.get(*k)? {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Array(items) if !items.is_empty() => Some(
                    items
                        .iter()
                        .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                _ => None,
            })
        };
        match (text(&["code", "error"]), text(&["message", "errorMessage", "error_description"])) {
            (Some(c), Some(m)) => return format!(" (HTTP {code}: {c} - {m})"),
            (None, Some(m)) => return format!(" (HTTP {code}: {m})"),
            (Some(c), None) => return format!(" (HTTP {code}: {c})"),
            (None, None) => {}
        }
    }
    let cleaned = body.trim();
    if !cleaned.is_empty()
        && cleaned.len() < 120
        && !cleaned.contains('\n')
        && !cleaned.starts_with('<')
    {
        return format!(" (HTTP {code}: {cleaned})");
    }
    format!(" (HTTP {code})")
}

fn first_str(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .map(str::to_owned)
}

/// Make a device from a REST device record. Return `None` for other device families.
///
/// # Examples
///
/// ```
/// use qubo_bridge::cloud::device_from_rest;
/// use qubo_bridge::models::DeviceKind;
/// use serde_json::json;
///
/// let item = json!({"deviceUUID": "d1", "aModelNo": "HPH01A", "deviceName": "Bedroom"});
/// let device = device_from_rest(&item, "unit-1", "user-1").expect("a purifier");
/// assert_eq!((device.kind, device.name.as_str()), (DeviceKind::Purifier, "Bedroom"));
/// // Other device families give None.
/// assert!(device_from_rest(&json!({"deviceUUID": "d2", "aModelNo": "HCP01"}), "u", "x").is_none());
/// ```
#[must_use]
pub fn device_from_rest(item: &Value, unit_uuid: &str, user_uuid: &str) -> Option<Device> {
    let model = first_str(item, &["aModelNo", "deviceModel"]).unwrap_or_default().to_uppercase();
    let kind = DeviceKind::ALL.into_iter().find(|kind| model.starts_with(kind.prefix()))?;
    let device_uuid = first_str(item, &["deviceUUID", "deviceId"])?;
    Some(Device {
        kind,
        mac: first_str(item, &["macAddress"]).unwrap_or_default(),
        entity_uuid: first_str(item, &["entityUUID"]).unwrap_or_else(|| device_uuid.clone()),
        unit_uuid: first_str(item, &["unitUUID"]).unwrap_or_else(|| unit_uuid.to_owned()),
        user_uuid: user_uuid.to_owned(),
        online: true,
        name: first_str(item, &["deviceName"]).unwrap_or_default(),
        device_uuid,
    })
}

/// Read the unit UUID from a unit record.
///
/// # Examples
///
/// ```
/// use qubo_bridge::cloud::extract_unit_uuid;
/// use serde_json::json;
///
/// assert_eq!(extract_unit_uuid(&json!({"unitId": "u1"})).as_deref(), Some("u1"));
/// ```
#[must_use]
pub fn extract_unit_uuid(unit: &Value) -> Option<String> {
    first_str(unit, &["unitUUID", "uuid", "unitId"])
}

fn trimmed(value: &Value) -> Value {
    value.as_str().map_or_else(|| value.clone(), |s| Value::String(s.trim().to_owned()))
}

/// Read `SHADOW_SERVICES` from the reported part of a shadow. The keys are nested
/// (`"lcSwitchControl": {"power": ..}`) or flat (`"lcSwitchControl_power": ..`).
///
/// # Examples
///
/// ```
/// use qubo_bridge::cloud::shadow_values;
/// use serde_json::json;
///
/// let reported = json!({"fanSpeedControl_speed": " 2"});
/// let reported = reported.as_object().cloned().unwrap_or_default();
/// let values = shadow_values(&reported);
/// assert_eq!(values["fanSpeedControl"]["speed"], "2");
/// ```
pub fn shadow_values(reported: &Map<String, Value>) -> HashMap<String, Map<String, Value>> {
    let mut out = HashMap::new();
    for service in SHADOW_SERVICES {
        let name = service.as_str();
        let mut values: Map<String, Value> =
            reported.get(name).and_then(Value::as_object).cloned().unwrap_or_default();
        let prefix = format!("{name}_");
        for (key, value) in reported {
            if let Some(field) = key.strip_prefix(&prefix) {
                values.insert(field.to_owned(), value.clone());
            }
        }
        if !values.is_empty() {
            out.insert(
                name.to_owned(),
                values.iter().map(|(k, v)| (k.clone(), trimmed(v))).collect(),
            );
        }
    }
    out
}

/// Collect the reported values from a shadow response.
fn reported_from_shadow(response: &Value) -> Map<String, Value> {
    let mut reported = Map::new();
    if let Some(services) = response.get("services").and_then(Value::as_array) {
        for entry in services {
            let Some(name) = entry.get("service").and_then(Value::as_str).filter(|s| !s.is_empty())
            else {
                continue;
            };
            let mut values = Map::new();
            for (key, raw) in
                entry.get("attributes").and_then(Value::as_object).into_iter().flatten()
            {
                let value = raw.get("value").unwrap_or(raw);
                if !value.is_null() {
                    values.insert(key.clone(), value.clone());
                    reported.insert(format!("{name}_{key}"), value.clone());
                }
            }
            reported.insert(name.to_owned(), Value::Object(values));
        }
    }
    let state = response.get("state");
    let legacy = state.and_then(|s| s.get("reported")).or_else(|| response.get("reported"));
    if let Some(Value::Object(legacy)) = legacy {
        reported.extend(legacy.clone());
    }
    reported
}

fn network_error(err: &reqwest::Error) -> Error {
    Error::Connection(format!("Network error: {err}"))
}

/// A login session with the QUBO cloud.
pub struct CloudSession {
    path: PathBuf,
    device_id: String,
    http: reqwest::Client,
    base_url: String,
    tokens: tokio::sync::Mutex<Option<Tokens>>,
    login_uuid: Mutex<Option<String>>,
}

impl CloudSession {
    /// Load the saved session, if there is one. A missing or bad file means: not logged in.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connection`] if the HTTP client cannot be built, or [`Error::Io`] and [`Error::Json`] if the saved configuration cannot be read.
    pub fn load(paths: &Paths) -> Result<Arc<Self>> {
        Self::with_base_url(paths, BASE_URL)
    }

    fn with_base_url(paths: &Paths, base_url: &str) -> Result<Arc<Self>> {
        let path = paths.session();
        let tokens = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Tokens>(&text).ok());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| network_error(&err))?;
        Ok(Arc::new(Self {
            device_id: device_id(&paths.config().with_file_name("device_id")),
            path,
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            tokens: tokio::sync::Mutex::new(tokens),
            login_uuid: Mutex::new(None),
        }))
    }

    /// Whether a login is saved.
    pub async fn logged_in(&self) -> bool {
        self.tokens.lock().await.as_ref().is_some_and(|t| !t.access_token.is_empty())
    }

    /// The UUID of the logged-in user.
    pub async fn user_uuid(&self) -> String {
        self.tokens.lock().await.as_ref().map(|t| t.user_uuid.clone()).unwrap_or_default()
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let text = serde_json::to_string(tokens)?;
        write_atomic(&self.path, text.as_bytes(), true)
    }

    async fn store(&self, tokens: Tokens) -> Result<()> {
        self.save(&tokens)?;
        *self.tokens.lock().await = Some(tokens);
        Ok(())
    }

    /// Delete the saved login.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the session file cannot be deleted.
    pub fn logout(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }

    async fn post(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<(StatusCode, String)> {
        let mut request = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("App-Id", APP_ID)
            .header("Source-Device-Id", &self.device_id)
            .json(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.map_err(|err| network_error(&err))?;
        let status = response.status();
        let text = response.text().await.map_err(|err| network_error(&err))?;
        Ok((status, text))
    }

    async fn post_ok(
        &self,
        what: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value> {
        let (status, text) = self.post(path, headers, body).await?;
        if status != StatusCode::OK {
            return Err(Error::Auth(format!("{what} failed{}", error_detail(status, &text))));
        }
        serde_json::from_str(&text)
            .map_err(|err| Error::Auth(format!("Invalid JSON returned by server: {err}")))
    }

    fn tokens_from(profile: &Value) -> Result<Tokens> {
        let access = first_str(profile, &["accessToken", "token"]);
        let user = first_str(profile, &["uuid", "userUUID", "userId"]);
        let (Some(access_token), Some(user_uuid)) = (access, user) else {
            return Err(Error::Auth("Missing required token fields in login response".to_owned()));
        };
        Ok(Tokens {
            refresh_token: first_str(profile, &["refreshToken"]),
            expires_at: Some(jwt_expiry(&access_token).unwrap_or_else(|| unix_now() + 86400.0)),
            access_token,
            user_uuid,
        })
    }

    /// Log in with the account e-mail address and password.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the account service rejects the login, and [`Error::Connection`] on a network failure.
    pub async fn login_email(&self, email: &str, password: &str) -> Result<()> {
        let path = format!("/sms/api/v1/sp/{SERVICE_PROVIDER}/user/login?system=CS");
        let body = json!({
            "username": email, "email": email, "password": password, "deviceAttribute": DEVICE_LABEL,
        });
        let headers = [("Source", "USER"), ("Token-Type", "USER")];
        let data = self.post_ok("Email login", &path, &headers, &body).await?;
        let profile = data.as_array().and_then(|list| list.first()).unwrap_or(&data);
        if !profile.is_object() {
            return Err(Error::Auth("Unexpected response format from login".to_owned()));
        }
        self.store(Self::tokens_from(profile)?).await
    }

    /// Ask for an SMS code.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the account service rejects the request, and [`Error::Connection`] on a network failure.
    pub async fn request_otp(&self, phone: &str, isd_code: &str) -> Result<()> {
        let path = format!("/sms/api/v1/sp/{SERVICE_PROVIDER}/user/phone/login");
        let body = json!({ "phone": phone, "isdCode": normal_isd(isd_code), "deviceAttribute": DEVICE_LABEL });
        let data = self.post_ok("OTP request", &path, &[("Source", "USER")], &body).await?;
        *self.login_uuid.lock().unwrap_or_else(PoisonError::into_inner) =
            first_str(&data, &["uuid"]);
        Ok(())
    }

    /// Log in with the SMS code.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the code is wrong or no code was requested, and [`Error::Connection`] on a network failure.
    pub async fn verify_otp(&self, phone: &str, otp: &str, isd_code: &str) -> Result<()> {
        let session_uuid = self
            .login_uuid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
            Error::Auth("Missing login session UUID. Request the SMS code first.".to_owned())
        })?;
        let path = format!("/sms/api/v1/sp/{SERVICE_PROVIDER}/user/phone/verify");
        let body = json!({
            "phone": phone, "isdCode": normal_isd(isd_code), "otp": otp,
            "deviceAttribute": DEVICE_LABEL, "uuid": session_uuid,
        });
        let data = self.post_ok("OTP verification", &path, &[("Source", "USER")], &body).await?;
        let profile = match &data {
            Value::Array(list) => list.first(),
            Value::Object(_) => Some(&data),
            _ => None,
        }
        .ok_or_else(|| {
            Error::Auth("No account profiles returned in OTP verification response".to_owned())
        })?;
        let mut tokens = Self::tokens_from(profile)?;
        // The OTP token lasts one hour and has no refresh token. A profile with an e-mail
        // address can swap it for a long-lived session.
        if profile.get("email").is_some_and(|e| e.as_str().is_some_and(|s| !s.is_empty())) {
            match self.finalize_phone_login(profile, &tokens).await {
                Ok(long_lived) => tokens = long_lived,
                Err(err) => {
                    warn!("Cannot get a refresh token ({err}). Using the OTP access token.");
                }
            }
        }
        self.store(tokens).await
    }

    async fn finalize_phone_login(&self, profile: &Value, temporary: &Tokens) -> Result<Tokens> {
        let identifier = first_str(profile, &["email", "username", "phone"])
            .ok_or_else(|| Error::Auth("Missing account identifier in OTP profile".to_owned()))?;
        let path = format!("/sms/api/v1/sp/{SERVICE_PROVIDER}/user/emailprofile/login");
        let headers = [
            ("Token-Type", "USER"),
            ("Source", "Android"),
            ("User-UUID", temporary.user_uuid.as_str()),
            ("Subscriber-Key", temporary.access_token.as_str()),
        ];
        let body = json!({ "username": identifier, "deviceAttribute": DEVICE_LABEL });
        let data = self.post_ok("Phone-login finalize", &path, &headers, &body).await?;
        let access_token = first_str(&data, &["accessToken", "token"]).ok_or_else(|| {
            Error::Auth("Missing access token in phone-login finalize response".to_owned())
        })?;
        Ok(Tokens {
            refresh_token: first_str(&data, &["refreshToken"]),
            expires_at: Some(jwt_expiry(&access_token).unwrap_or_else(|| unix_now() + 86400.0)),
            access_token,
            user_uuid: temporary.user_uuid.clone(),
        })
    }

    /// Renew the access token with the refresh token.
    ///
    /// With `failed`, renew only if that token is still the current one.
    async fn refresh_session(&self, failed: Option<&str>) -> Result<()> {
        let mut guard = self.tokens.lock().await;
        let no_refresh = || Error::Auth("No refresh token available to renew session".to_owned());
        let current = guard.clone().ok_or_else(no_refresh)?;
        let refresh_token = current.refresh_token.clone().ok_or_else(no_refresh)?;
        match failed {
            Some(token) if current.access_token != token => return Ok(()),
            None if current.expires_at.is_some_and(|e| e - unix_now() > RENEW_MARGIN) => {
                return Ok(());
            }
            _ => {}
        }
        let path =
            format!("/sms/api/v1/sp/{SERVICE_PROVIDER}/users/{}/auth/refresh", current.user_uuid);
        let bearer = format!("Bearer {}", current.access_token);
        let headers = [
            ("Authorization", bearer.as_str()),
            ("Subscriber-Key", current.access_token.as_str()),
            ("User-UUID", current.user_uuid.as_str()),
            ("Token-Type", "USER"),
            ("Source", "Android"),
        ];
        let body = json!({ "accessToken": current.access_token, "refreshToken": refresh_token });
        let (status, text) = self.post(&path, &headers, &body).await?;
        if status != StatusCode::OK {
            let detail = error_detail(status, &text);
            if matches!(status.as_u16(), 400 | 401 | 403) {
                *guard = None;
                return Err(Error::Auth(format!(
                    "Session refresh rejected{detail}: refresh token expired or invalid; re-login required"
                )));
            }
            return Err(Error::Auth(format!("Session refresh failed{detail}")));
        }
        let data: Value = serde_json::from_str(&text).map_err(|err| {
            Error::Auth(format!("Invalid JSON returned by server during refresh: {err}"))
        })?;
        let access_token = first_str(&data, &["accessToken", "access_token"])
            .ok_or_else(|| Error::Auth("Refresh response missing access token".to_owned()))?;
        let renewed = Tokens {
            refresh_token: first_str(&data, &["refreshToken", "refresh_token"])
                .or(Some(refresh_token)),
            expires_at: Some(jwt_expiry(&access_token).unwrap_or_else(|| unix_now() + 86400.0)),
            access_token,
            user_uuid: current.user_uuid,
        };
        self.save(&renewed)?;
        *guard = Some(renewed);
        drop(guard);
        Ok(())
    }

    /// Return a valid access token. Renew it first if it expires soon.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if there is no login, or the session expired and cannot be renewed.
    pub async fn token(&self) -> Result<String> {
        let current =
            self.tokens.lock().await.clone().ok_or_else(|| {
                Error::Auth("Authentication required: No tokens present".to_owned())
            })?;
        let now = unix_now();
        let expires_soon = current.expires_at.is_some_and(|e| e - now < RENEW_MARGIN);
        if expires_soon && current.refresh_token.is_some() {
            if let Err(err) = self.refresh_session(None).await {
                let after = self.tokens.lock().await.clone();
                if after.as_ref().is_none_or(|t| t.expires_at.is_some_and(|e| e <= now)) {
                    return Err(Error::Auth(format!(
                        "Session expired and automatic token renewal failed: {err}"
                    )));
                }
                warn!("Proactive token refresh failed: {err}");
            }
        } else if current.expires_at.is_some_and(|e| e <= now) {
            return Err(Error::Auth("Session expired; please log in again".to_owned()));
        }
        let token = self.tokens.lock().await.as_ref().map(|t| t.access_token.clone());
        token.ok_or_else(|| Error::Auth("Authentication required: No tokens present".to_owned()))
    }

    /// Send an authenticated GET request. After a 401 answer, renew the token and try once more.
    async fn get(&self, path: &str) -> Result<Value> {
        for attempt in 0..2 {
            let token = self.token().await?;
            let user = self.user_uuid().await;
            let response = self
                .http
                .request(Method::GET, format!("{}{path}", self.base_url))
                .header("Authorization", format!("Bearer {token}"))
                .header("Token-Type", "USER")
                .header("User-UUID", user)
                .header("Subscriber-Key", &token)
                .header("Source-Device-Id", &self.device_id)
                .header("App-Id", APP_ID)
                .header("Source", "Android")
                .send()
                .await
                .map_err(|err| network_error(&err))?;
            let status = response.status();
            let text = response.text().await.map_err(|err| network_error(&err))?;
            let can_retry = attempt == 0
                && status == StatusCode::UNAUTHORIZED
                && self.tokens.lock().await.as_ref().is_some_and(|t| t.refresh_token.is_some());
            if can_retry {
                self.refresh_session(Some(&token)).await?;
                continue;
            }
            return match status.as_u16() {
                401 | 403 => Err(Error::Auth(format!(
                    "Authentication rejected by Qubo API (HTTP {})",
                    status.as_u16()
                ))),
                200 | 201 if !text.is_empty() => Ok(serde_json::from_str(&text)
                    .unwrap_or_else(|_| json!({ "status": text.trim() }))),
                200 | 201 | 204 => Ok(json!({})),
                code => {
                    Err(Error::Connection(format!("Qubo API error (HTTP {code}) on GET {path}")))
                }
            };
        }
        Err(Error::Auth("Authentication rejected by Qubo API (HTTP 401)".to_owned()))
    }

    /// Get a list from the gateway. Any failure is an error, so that a network problem
    /// never looks like an account with no devices.
    async fn list(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        Ok(match self.get(path).await? {
            Value::Array(items) => items,
            Value::Object(map) => match map.get("data").or_else(|| map.get(key)) {
                Some(Value::Array(items)) => items.clone(),
                _ => vec![Value::Object(map)],
            },
            _ => Vec::new(),
        })
    }

    /// The devices of all home units of the account.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the login is rejected or missing, and [`Error::Connection`] on a network failure.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let user = self.user_uuid().await;
        let mut found = Vec::new();
        let units = self
            .list(&format!("/unit-entity-management/api/v1/sp/{SERVICE_PROVIDER}/units"), "units")
            .await?;
        for unit in units {
            let Some(unit_uuid) = extract_unit_uuid(&unit) else {
                continue;
            };
            let path = format!(
                "/unit-entity-management/api/v1/sp/{SERVICE_PROVIDER}/units/{unit_uuid}/devices"
            );
            for item in self.list(&path, "devices").await? {
                found.extend(device_from_rest(&item, &unit_uuid, &user));
            }
        }
        Ok(found)
    }

    /// The last reported values of a device for the services that report only on change.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if the login is rejected or missing, and [`Error::Connection`] on a network failure.
    pub async fn shadow(&self, device: &Device) -> Result<HashMap<String, Map<String, Value>>> {
        let path = if device.unit_uuid.is_empty() {
            format!(
                "/unit-entity-management/api/v2/sp/{SERVICE_PROVIDER}/devices/{}/deviceshadow",
                device.device_uuid
            )
        } else {
            format!(
                "/unit-entity-management/api/v1/sp/{SERVICE_PROVIDER}/units/{}/entities/{}/devices/{}/shadow",
                device.unit_uuid, device.entity_uuid, device.device_uuid
            )
        };
        Ok(shadow_values(&reported_from_shadow(&self.get(&path).await?)))
    }

    /// Settings for a client of the cloud broker. It asks for a new token before each connect.
    pub async fn client_config(
        self: &Arc<Self>,
        devices: Vec<Device>,
        cache: Option<PathBuf>,
    ) -> ClientConfig {
        let session = Arc::clone(self);
        let credentials = Credentials {
            username: self.user_uuid().await,
            token: Arc::new(move || -> TokenFuture {
                let session = Arc::clone(&session);
                Box::pin(async move { session.token().await })
            }),
        };
        ClientConfig::cloud(devices, cache, credentials)
    }
}

fn normal_isd(isd_code: &str) -> String {
    if isd_code.starts_with('+') { isd_code.to_owned() } else { format!("+{isd_code}") }
}

/// Read the identifier of this installation, or make one. The cloud shows it as the session.
fn device_id(path: &std::path::Path) -> String {
    if let Ok(saved) = std::fs::read_to_string(path)
        && !saved.trim().is_empty()
    {
        return saved.trim().to_owned();
    }
    let id = uuid::Uuid::new_v4().to_string();
    // The identifier is optional. Without a file, each run uses a new one.
    let _ = write_atomic(path, id.as_bytes(), true);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn purifier_record() -> Value {
        json!({
            "deviceUUID": "d1", "entityUUID": "e1", "aModelNo": "HPH01A",
            "macAddress": "AA:BB", "deviceName": "Bedroom",
        })
    }

    fn purifier() -> Device {
        device_from_rest(&purifier_record(), "u1", "user-1").unwrap()
    }

    mod device_from_rest {
        use super::*;

        #[test]
        fn reads_the_kind_from_the_model_number() {
            assert_eq!(purifier().kind, DeviceKind::Purifier);
        }

        #[test]
        fn takes_the_unit_uuid_from_the_caller_when_the_record_has_none() {
            assert_eq!(purifier().unit_uuid, "u1");
        }

        #[test]
        fn takes_the_user_uuid_from_the_caller() {
            assert_eq!(purifier().user_uuid, "user-1");
        }

        #[test]
        fn reads_the_device_name() {
            assert_eq!(purifier().name, "Bedroom");
        }

        #[test]
        fn skips_another_device_family() {
            let record = json!({ "deviceUUID": "d2", "aModelNo": "HCP01" });
            assert!(device_from_rest(&record, "u1", "x").is_none());
        }

        #[test]
        fn skips_a_record_without_a_device_id() {
            assert!(device_from_rest(&json!({ "aModelNo": "HSP02" }), "u1", "x").is_none());
        }

        #[test]
        fn reads_the_kind_from_a_lower_case_model_number() {
            let record = json!({ "deviceId": "d3", "deviceModel": "hsp02" });
            assert_eq!(device_from_rest(&record, "u1", "x").unwrap().kind, DeviceKind::Socket);
        }

        #[test]
        fn uses_the_device_uuid_as_the_entity_uuid_when_the_record_has_none() {
            let record = json!({ "deviceId": "d3", "deviceModel": "hsp02" });
            assert_eq!(device_from_rest(&record, "u1", "x").unwrap().entity_uuid, "d3");
        }
    }

    mod extract_unit_uuid {
        use super::*;

        #[test]
        fn reads_the_unit_uuid_key() {
            assert_eq!(extract_unit_uuid(&json!({ "unitUUID": "u" })).as_deref(), Some("u"));
        }

        #[test]
        fn reads_the_unit_id_key() {
            assert_eq!(extract_unit_uuid(&json!({ "unitId": "u" })).as_deref(), Some("u"));
        }

        #[test]
        fn returns_none_for_an_empty_record() {
            assert_eq!(extract_unit_uuid(&json!({})), None);
        }
    }

    mod shadow_values {
        use super::*;

        fn reported() -> Map<String, Value> {
            obj(json!({
                "lcSwitchControl": { "power": "on" },
                "fanSpeedControl_speed": " 2",
                "aqiStatus_PM25": "9",
            }))
        }

        #[test]
        fn reads_a_nested_service() {
            assert_eq!(
                shadow_values(&reported())["lcSwitchControl"],
                obj(json!({ "power": "on" }))
            );
        }

        #[test]
        fn reads_a_flat_key_and_trims_the_value() {
            assert_eq!(shadow_values(&reported())["fanSpeedControl"], obj(json!({ "speed": "2" })));
        }

        #[test]
        fn leaves_out_services_that_report_on_demand() {
            assert!(!shadow_values(&reported()).contains_key("aqiStatus"));
        }
    }

    mod reported_from_shadow {
        use super::*;

        #[test]
        fn flattens_a_services_array() {
            let response = json!({ "services": [{
                "service": "fanSpeedControl", "attributes": { "speed": { "value": "3" } }
            }] });
            let values = shadow_values(&reported_from_shadow(&response));
            assert_eq!(values["fanSpeedControl"], obj(json!({ "speed": "3" })));
        }

        #[test]
        fn reads_the_reported_part_of_a_legacy_state() {
            let response = json!({ "state": { "reported": { "lcSwitchControl_power": "off" } } });
            let values = shadow_values(&reported_from_shadow(&response));
            assert_eq!(values["lcSwitchControl"], obj(json!({ "power": "off" })));
        }
    }

    mod jwt_expiry {
        use super::*;

        #[test]
        fn reads_the_exp_claim() {
            let payload = URL_SAFE_NO_PAD.encode(r#"{"exp": 4102444800}"#);
            assert_eq!(jwt_expiry(&format!("h.{payload}.s")), Some(4_102_444_800.0));
        }

        #[test]
        fn returns_none_for_text_that_is_not_a_token() {
            assert_eq!(jwt_expiry("not-a-token"), None);
        }
    }

    mod error_detail {
        use super::*;

        #[test]
        fn joins_the_code_and_the_message() {
            let body = r#"{"code": "E1", "message": "bad"}"#;
            assert_eq!(error_detail(StatusCode::BAD_REQUEST, body), " (HTTP 400: E1 - bad)");
        }

        #[test]
        fn hides_an_html_body() {
            assert_eq!(error_detail(StatusCode::BAD_GATEWAY, "<html>"), " (HTTP 502)");
        }
    }

    mod session {
        use super::*;

        fn tokens() -> Tokens {
            Tokens {
                access_token: "tok".to_owned(),
                refresh_token: Some("ref".to_owned()),
                user_uuid: "user-1".to_owned(),
                expires_at: Some(4e9),
            }
        }

        fn paths(dir: &tempfile::TempDir) -> Paths {
            Paths::new(dir.path().join("config.json"))
        }

        async fn saved(dir: &tempfile::TempDir) -> Arc<CloudSession> {
            let session = CloudSession::load(&paths(dir)).unwrap();
            session.store(tokens()).await.unwrap();
            CloudSession::load(&paths(dir)).unwrap()
        }

        #[tokio::test]
        async fn starts_logged_out_without_a_session_file() {
            let dir = tempfile::tempdir().unwrap();
            assert!(!CloudSession::load(&paths(&dir)).unwrap().logged_in().await);
        }

        #[tokio::test]
        async fn stays_logged_in_after_a_reload() {
            let dir = tempfile::tempdir().unwrap();
            assert!(saved(&dir).await.logged_in().await);
        }

        #[tokio::test]
        async fn keeps_the_user_uuid_after_a_reload() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(saved(&dir).await.user_uuid().await, "user-1");
        }

        #[tokio::test]
        async fn returns_the_saved_token_without_a_network_call_when_it_is_valid() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(saved(&dir).await.token().await.unwrap(), "tok");
        }

        #[tokio::test]
        async fn deletes_the_session_file_on_logout() {
            let dir = tempfile::tempdir().unwrap();
            saved(&dir).await.logout().unwrap();
            assert!(!paths(&dir).session().exists());
        }
    }

    /// A request that the mock gateway received.
    struct Hit {
        method: String,
        path: String,
        /// The request line and headers, in lower case.
        head: String,
        body: String,
    }

    struct Mock {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
    }

    /// A local HTTP server. The handler gives the status and the body for each request.
    async fn mock(handler: impl Fn(&Hit) -> (u16, String) + Send + Sync + 'static) -> Mock {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let (head_end, body_len) = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (end + 4, len);
                        }
                    };
                    while buf.len() < head_end + body_len {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let mut line = head.lines().next().unwrap_or_default().split_whitespace();
                    let hit = Hit {
                        method: line.next().unwrap_or_default().to_owned(),
                        path: line.next().unwrap_or_default().to_owned(),
                        head: head.to_lowercase(),
                        body: String::from_utf8_lossy(&buf[head_end..head_end + body_len])
                            .into_owned(),
                    };
                    log.lock().unwrap().push(format!("{} {}", hit.method, hit.path));
                    let (status, body) = handler(&hit);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Mock { base, hits }
    }

    mod http {
        use super::*;

        fn tokens(access: &str, refresh: Option<&str>, expires_at: f64) -> Tokens {
            Tokens {
                access_token: access.to_owned(),
                refresh_token: refresh.map(str::to_owned),
                user_uuid: "user-1".to_owned(),
                expires_at: Some(expires_at),
            }
        }

        async fn session_for(
            mock: &Mock,
            saved: Option<Tokens>,
        ) -> (tempfile::TempDir, Paths, Arc<CloudSession>) {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path().join("config.json"));
            let session = CloudSession::with_base_url(&paths, &mock.base).unwrap();
            if let Some(saved) = saved {
                session.store(saved).await.unwrap();
            }
            (dir, paths, session)
        }

        fn json_ok(body: &str) -> (u16, String) {
            (200, body.to_owned())
        }

        /// A gateway that accepts only the token "new" and renews "old" into "new".
        fn renewing_gateway(hit: &Hit) -> (u16, String) {
            if hit.path.contains("/auth/refresh") {
                return json_ok(r#"{"accessToken":"new","refreshToken":"ref2"}"#);
            }
            if !hit.head.contains("bearer new") {
                return (401, String::new());
            }
            if hit.path.ends_with("/units") {
                json_ok(r#"[{"unitUUID":"u1"}]"#)
            } else {
                json_ok(&format!("[{}]", purifier_record()))
            }
        }

        mod login_email {
            use super::*;

            async fn logged_in() -> (tempfile::TempDir, Paths, Arc<CloudSession>) {
                let gateway = mock(|hit| {
                    if hit.body.contains(r#""password":"pw""#) {
                        json_ok(r#"{"accessToken":"a","uuid":"u","refreshToken":"r"}"#)
                    } else {
                        (400, r#"{"message":"bad password"}"#.to_owned())
                    }
                })
                .await;
                let (dir, paths, session) = session_for(&gateway, None).await;
                session.login_email("a@b.c", "pw").await.unwrap();
                (dir, paths, session)
            }

            #[tokio::test]
            async fn sends_the_password_in_the_body() {
                let (_dir, _paths, session) = logged_in().await;
                assert!(session.logged_in().await);
            }

            #[tokio::test]
            async fn keeps_the_access_token() {
                let (_dir, _paths, session) = logged_in().await;
                assert_eq!(session.token().await.unwrap(), "a");
            }

            #[tokio::test]
            async fn saves_the_session_file() {
                let (_dir, paths, _session) = logged_in().await;
                let text = std::fs::read_to_string(paths.session()).unwrap();
                assert!(text.contains(r#""user_uuid":"u""#), "{text}");
            }

            #[tokio::test]
            async fn fails_with_the_message_of_the_server() {
                let gateway = mock(|_| (400, r#"{"message":"bad password"}"#.to_owned())).await;
                let (_dir, _paths, session) = session_for(&gateway, None).await;
                let err = session.login_email("a@b.c", "wrong").await.unwrap_err();
                assert!(matches!(err, Error::Auth(ref m) if m.contains("bad password")), "{err}");
            }
        }

        mod phone_otp {
            use super::*;

            fn gateway(
                profile: &'static str,
                finalize: (u16, &'static str),
            ) -> impl Fn(&Hit) -> (u16, String) {
                move |hit| {
                    if hit.path.ends_with("/phone/login") {
                        json_ok(r#"{"uuid":"sess"}"#)
                    } else if hit.path.ends_with("/phone/verify") {
                        json_ok(profile)
                    } else {
                        (finalize.0, finalize.1.to_owned())
                    }
                }
            }

            async fn token_after_otp(
                profile: &'static str,
                finalize: (u16, &'static str),
            ) -> String {
                let gateway = mock(gateway(profile, finalize)).await;
                let (_dir, _paths, session) = session_for(&gateway, None).await;
                session.request_otp("123", "91").await.unwrap();
                session.verify_otp("123", "0000", "91").await.unwrap();
                session.token().await.unwrap()
            }

            #[tokio::test]
            async fn keeps_the_otp_token_when_the_profile_has_no_email() {
                let profile = r#"[{"token":"short","uuid":"u"}]"#;
                assert_eq!(token_after_otp(profile, (500, "")).await, "short");
            }

            #[tokio::test]
            async fn swaps_the_otp_token_for_a_long_lived_one_when_the_profile_has_an_email() {
                let profile = r#"[{"token":"short","uuid":"u","email":"a@b.c"}]"#;
                let long = (200, r#"{"accessToken":"long","refreshToken":"r"}"#);
                assert_eq!(token_after_otp(profile, long).await, "long");
            }

            #[tokio::test]
            async fn keeps_the_otp_token_when_the_swap_fails() {
                let profile = r#"[{"token":"short","uuid":"u","email":"a@b.c"}]"#;
                assert_eq!(token_after_otp(profile, (500, "")).await, "short");
            }

            #[tokio::test]
            async fn refuses_a_code_when_none_was_requested() {
                let gateway = mock(|_| json_ok("{}")).await;
                let (_dir, _paths, session) = session_for(&gateway, None).await;
                let err = session.verify_otp("123", "0000", "91").await.unwrap_err();
                assert!(
                    matches!(err, Error::Auth(ref m) if m.contains("Missing login session")),
                    "{err}"
                );
            }
        }

        mod token {
            use super::*;

            #[tokio::test]
            async fn makes_no_request_while_the_token_is_valid() {
                let gateway = mock(renewing_gateway).await;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), 4e9))).await;
                session.token().await.unwrap();
                assert!(gateway.hits.lock().unwrap().is_empty());
            }

            #[tokio::test]
            async fn renews_a_token_that_expires_soon() {
                let gateway = mock(renewing_gateway).await;
                let soon = unix_now() + 10.0;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), soon))).await;
                assert_eq!(session.token().await.unwrap(), "new");
            }

            #[tokio::test]
            async fn fails_when_the_token_expired_and_there_is_no_refresh_token() {
                let gateway = mock(renewing_gateway).await;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", None, 1.0))).await;
                let err = session.token().await.unwrap_err();
                assert!(
                    matches!(err, Error::Auth(ref m) if m == "Session expired; please log in again"),
                    "{err}"
                );
            }
        }

        mod devices {
            use super::*;

            async fn after_a_401() -> (tempfile::TempDir, Paths, Arc<CloudSession>, Vec<Device>) {
                let gateway = mock(renewing_gateway).await;
                let (dir, paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), 4e9))).await;
                let found = session.devices().await.unwrap();
                (dir, paths, session, found)
            }

            #[tokio::test]
            async fn retries_with_a_renewed_token_after_a_401() {
                let (_dir, _paths, _session, found) = after_a_401().await;
                assert_eq!(found.len(), 1);
            }

            #[tokio::test]
            async fn keeps_the_renewed_token() {
                let (_dir, _paths, session, _found) = after_a_401().await;
                assert_eq!(session.token().await.unwrap(), "new");
            }

            #[tokio::test]
            async fn saves_the_renewed_token_in_the_session_file() {
                let (_dir, paths, _session, _found) = after_a_401().await;
                let text = std::fs::read_to_string(paths.session()).unwrap();
                assert!(text.contains(r#""access_token":"new""#), "{text}");
            }

            fn rejecting(hit: &Hit) -> (u16, String) {
                if hit.path.contains("/auth/refresh") {
                    (401, r#"{"message":"expired"}"#.to_owned())
                } else {
                    (401, String::new())
                }
            }

            #[tokio::test]
            async fn fails_with_a_re_login_message_when_the_refresh_is_rejected() {
                let gateway = mock(rejecting).await;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), 4e9))).await;
                let err = session.devices().await.unwrap_err();
                assert!(
                    matches!(err, Error::Auth(ref m) if m.contains("re-login required")),
                    "{err}"
                );
            }

            #[tokio::test]
            async fn clears_the_login_when_the_refresh_is_rejected() {
                let gateway = mock(rejecting).await;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), 4e9))).await;
                let _ = session.devices().await;
                assert!(!session.logged_in().await);
            }

            #[tokio::test]
            async fn returns_the_error_when_the_gateway_fails() {
                let gateway = mock(|_| (500, String::new())).await;
                let (_dir, _paths, session) =
                    session_for(&gateway, Some(tokens("old", Some("r"), 4e9))).await;
                let err = session.devices().await.unwrap_err();
                assert!(matches!(err, Error::Connection(ref m) if m.contains("HTTP 500")), "{err}");
            }
        }
    }

    mod tokens {
        use super::*;

        #[test]
        fn does_not_print_the_access_token() {
            let tokens = Tokens {
                access_token: "secret".to_owned(),
                refresh_token: None,
                user_uuid: "u".to_owned(),
                expires_at: None,
            };
            assert!(!format!("{tokens:?}").contains("secret"));
        }
    }
}
