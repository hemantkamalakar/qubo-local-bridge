//! MCP server (`qubo mcp`): QUBO purifier and socket tools for AI assistants.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

use crate::client::{ControlPath, QuboClient};
use crate::connect::connect;
use crate::error::{Error, Result as QuboResult};
use crate::models::{Config, DeviceKind, Paths, find_device};
use crate::protocol::{self, Command, FanMode};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest wait that a tool call can ask for.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// The wait for a tool call. A missing, negative or invalid value gives the default.
fn wait_from(seconds: Option<f64>) -> Duration {
    seconds
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .map_or(REPLY_TIMEOUT, |wait| wait.min(MAX_WAIT))
}

/// The MCP server. It keeps one long-lived broker connection.
#[derive(Clone)]
pub struct QuboServer {
    paths: Paths,
    client: Arc<OnceCell<Arc<QuboClient>>>,
    tool_router: ToolRouter<Self>,
}

/// Parameters of `refresh_sensors`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefreshParams {
    /// Seconds to wait for the readings. Default 5.
    timeout: Option<f64>,
}

/// Parameters of `control_purifier`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PurifierParams {
    /// Switch the purifier on or off.
    power: Option<bool>,
    /// Fan speed: 1 (low) to 3 (high).
    speed: Option<i64>,
    /// Fan mode.
    mode: Option<FanMode>,
    /// Name, MAC or UUID. Leave it out when there is one purifier.
    device: Option<String>,
}

/// Parameters of `control_socket`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SocketParams {
    /// Switch the socket on or off.
    power: bool,
    /// Switch off even above the load limit. Set it only after the user confirms.
    force: Option<bool>,
    /// Name, MAC or UUID. Leave it out when there is one socket.
    device: Option<String>,
}

/// Turn a result into a tool result. A failure becomes a tool error, not a protocol error.
fn reply(result: QuboResult<Value>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
    }
}

fn rows(client: &QuboClient) -> Value {
    Value::Array(client.telemetry().into_iter().map(Value::Object).collect())
}

#[tool_router]
impl QuboServer {
    /// Make a server. It connects when the first tool runs.
    #[must_use]
    pub fn new(paths: Paths) -> Self {
        Self { paths, client: Arc::new(OnceCell::new()), tool_router: Self::tool_router() }
    }

    async fn client(&self) -> QuboResult<&Arc<QuboClient>> {
        self.client
            .get_or_try_init(|| async {
                let config = Config::load(&self.paths)?;
                let client = connect(&self.paths, &config, None, REPLY_TIMEOUT).await?;
                // The broker of 'qubo serve' refreshes the sensors. On the cloud path this process does.
                if client.path() == ControlPath::Cloud {
                    client.start_auto_refresh();
                }
                Ok(Arc::new(client))
            })
            .await
    }

    async fn purifier(&self, params: PurifierParams) -> QuboResult<Value> {
        let client = self.client().await?;
        let devices = client.devices();
        let device = find_device(&devices, DeviceKind::Purifier, params.device.as_deref())?;
        let mut commands: Vec<Command> = Vec::new();
        if let Some(on) = params.power {
            commands.push(protocol::power(device, on));
        }
        if let Some(speed) = params.speed {
            commands.push(protocol::fan_speed(device, speed)?);
        }
        if let Some(mode) = params.mode {
            commands.push(protocol::fan_mode(device, mode));
        }
        if commands.is_empty() {
            return Err(Error::Value("Give power, speed or mode.".to_owned()));
        }
        let mut confirmed = Map::new();
        for command in &commands {
            let sent = client.send(command, REPLY_TIMEOUT).await?;
            confirmed.insert(command.reply.to_string(), Value::Bool(sent));
        }
        Ok(json!({ "device": device.label(), "path": client.path(), "confirmed": confirmed }))
    }

    async fn socket(&self, params: SocketParams) -> QuboResult<Value> {
        let client = self.client().await?;
        let devices = client.devices();
        let device = find_device(&devices, DeviceKind::Socket, params.device.as_deref())?;
        let sent = client
            .switch_socket(device, params.power, params.force.unwrap_or(false), REPLY_TIMEOUT)
            .await?;
        Ok(json!({ "device": device.label(), "path": client.path(), "confirmed": sent }))
    }

    #[tool(
        description = "Return the latest state of every purifier and socket. A null value is not reported yet. Fields in \"cached\" come from an earlier session."
    )]
    async fn get_home_telemetry(&self) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(reply(self.client().await.map(|client| rows(client))))
    }

    #[tool(
        description = "Ask all devices for fresh PM2.5, filter-life and energy readings. Return telemetry."
    )]
    async fn refresh_sensors(
        &self,
        Parameters(params): Parameters<RefreshParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let wait = wait_from(params.timeout);
        let result = async {
            let client = self.client().await?;
            client.refresh(wait).await?;
            Ok(rows(client))
        }
        .await;
        Ok(reply(result))
    }

    #[tool(
        description = "Change an air purifier. speed: 1 (low) to 3 (high). confirmed is false when the device did not report the change in 5 s."
    )]
    async fn control_purifier(
        &self,
        Parameters(params): Parameters<PurifierParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(reply(self.purifier(params).await))
    }

    #[tool(
        description = "Switch a smart socket on or off. Switch-off is refused when the load draws more than 15 W or gives no fresh reading. Set force to true only after the user confirms."
    )]
    async fn control_socket(
        &self,
        Parameters(params): Parameters<SocketParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(reply(self.socket(params).await))
    }
}

// The `tool_handler` macro generates async fns with no `.await`.
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for QuboServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("qubo", env!("CARGO_PKG_VERSION")))
            .with_instructions(
            "Control of QUBO air purifiers and smart sockets, on the local network or the cloud. \
             Call get_home_telemetry before you change a device.",
        )
    }
}

fn transport_error(err: impl std::fmt::Display) -> Error {
    Error::Connection(format!("MCP server stopped: {err}"))
}

/// Serve MCP on standard input and output until the client closes it.
///
/// # Errors
///
/// Returns [`Error::Connection`] if the MCP transport stops with an error.
pub async fn run_stdio(server: QuboServer) -> QuboResult<()> {
    let running = server.serve(rmcp::transport::stdio()).await.map_err(transport_error)?;
    running.waiting().await.map_err(transport_error)?;
    Ok(())
}

/// Serve MCP over streamable HTTP at `/mcp` until Ctrl+C.
///
/// # Errors
///
/// Returns [`Error::Io`] if the address cannot be bound or the server fails, and [`Error::Connection`] if the MCP transport stops.
pub async fn run_http(server: QuboServer, host: &str, port: u16) -> QuboResult<()> {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, router).with_graceful_shutdown(stop).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::testing::{device, meter, offline, socket, switch_reply};

    fn server_with(client: QuboClient) -> QuboServer {
        let server = QuboServer::new(Paths::new("/unused/config.json"));
        assert!(server.client.set(Arc::new(client)).is_ok());
        server
    }

    fn off(force: Option<bool>) -> SocketParams {
        SocketParams { power: false, force, device: None }
    }

    fn purifier_speed(speed: Option<i64>) -> PurifierParams {
        PurifierParams { power: None, speed, mode: None, device: None }
    }

    fn speed_reply() -> crate::client::testing::Reply {
        ("fanSpeedControl", ("fanSpeedControl", json!({ "speed": "2" })))
    }

    mod control_socket {
        use super::*;

        #[tokio::test]
        async fn confirms_a_switch_on() {
            let (client, _) = offline(vec![socket()], None, vec![switch_reply()]);
            let params = SocketParams { power: true, force: None, device: None };
            let result = server_with(client).socket(params).await.unwrap();
            assert_eq!(result["confirmed"], true);
        }

        #[tokio::test]
        async fn names_the_device_in_the_result() {
            let (client, _) = offline(vec![socket()], None, vec![switch_reply()]);
            let params = SocketParams { power: true, force: None, device: None };
            let result = server_with(client).socket(params).await.unwrap();
            assert_eq!(result["device"], "desk");
        }

        #[tokio::test]
        async fn refuses_a_switch_off_above_the_load_limit() {
            let (client, _) = offline(vec![socket()], None, vec![meter("40"), switch_reply()]);
            let result = server_with(client).socket(off(None)).await;
            assert!(matches!(result, Err(Error::Interlock(_))));
        }

        #[tokio::test]
        async fn switches_off_above_the_limit_with_force() {
            let (client, _) = offline(vec![socket()], None, vec![meter("40"), switch_reply()]);
            let result = server_with(client).socket(off(Some(true))).await.unwrap();
            assert_eq!(result["confirmed"], true);
        }

        #[tokio::test]
        async fn fails_when_no_socket_matches_the_filter() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            let params =
                SocketParams { power: true, force: None, device: Some("other".to_owned()) };
            let result = server_with(client).socket(params).await;
            assert!(matches!(result, Err(Error::Lookup(_))));
        }
    }

    mod control_purifier {
        use super::*;

        fn purifier_client() -> QuboClient {
            let purifier = device(DeviceKind::Purifier, "dev-s");
            offline(vec![purifier], None, vec![speed_reply()]).0
        }

        #[tokio::test]
        async fn confirms_a_speed_change() {
            let result =
                server_with(purifier_client()).purifier(purifier_speed(Some(2))).await.unwrap();
            assert_eq!(result["confirmed"]["fanSpeedControl"], true);
        }

        #[tokio::test]
        async fn refuses_a_call_without_a_change() {
            let result = server_with(purifier_client()).purifier(purifier_speed(None)).await;
            assert!(
                matches!(result, Err(Error::Value(ref m)) if m == "Give power, speed or mode.")
            );
        }

        #[tokio::test]
        async fn refuses_a_speed_above_3() {
            let result = server_with(purifier_client()).purifier(purifier_speed(Some(4))).await;
            assert!(matches!(result, Err(Error::Value(_))));
        }
    }

    mod rows {
        use super::*;

        #[tokio::test]
        async fn gives_one_row_for_each_device() {
            let (client, _) = offline(vec![socket()], None, vec![]);
            assert_eq!(rows(&client).as_array().map(Vec::len), Some(1));
        }
    }

    mod get_info {
        use super::*;

        #[test]
        fn names_the_server_qubo() {
            let server = QuboServer::new(Paths::new("/unused/config.json"));
            assert_eq!(server.get_info().server_info.name, "qubo");
        }
    }

    mod wait_from {
        use super::*;

        #[test]
        fn uses_the_default_when_no_value_is_given() {
            assert_eq!(wait_from(None), Duration::from_secs(5));
        }

        #[test]
        fn keeps_a_valid_value() {
            assert_eq!(wait_from(Some(2.5)), Duration::from_millis(2500));
        }

        #[test]
        fn caps_a_huge_value_at_60_seconds() {
            assert_eq!(wait_from(Some(1e9)), Duration::from_secs(60));
        }

        #[test]
        fn uses_the_default_for_a_negative_value() {
            assert_eq!(wait_from(Some(-1.0)), Duration::from_secs(5));
        }

        #[test]
        fn uses_the_default_for_not_a_number() {
            assert_eq!(wait_from(Some(f64::NAN)), Duration::from_secs(5));
        }
    }
}
