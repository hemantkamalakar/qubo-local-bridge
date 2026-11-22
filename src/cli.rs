//! The `qubo` command line.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use comfy_table::Table;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use serde_json::{Map, Value, json};
use tracing_subscriber::EnvFilter;

use crate::broker::{BrokerConfig, DEVICE_PORT};
use crate::client::{ControlPath, QuboClient};
use crate::cloud::CloudSession;
use crate::connect::connect;
use crate::error::{Error, Result};
use crate::models::{Config, Device, DeviceKind, Mode, Paths, find_device};
use crate::protocol::{self, FanMode, Service, dumps};
use crate::{doctor, mcp_server, serve, service};

/// Control QUBO purifiers and sockets, local or cloud.
#[derive(Debug, Parser)]
#[command(name = "qubo", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands of `qubo`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in to the QUBO account (cloud path) and save the devices.
    Login(LoginArgs),
    /// Delete the saved cloud login.
    Logout(Json),
    /// Run the local broker for the local path. Stop with Ctrl+C.
    Serve(ServeArgs),
    /// Start 'qubo serve' at boot.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Check the cloud login, the local broker, the DNS record and the devices.
    Doctor(Json),
    /// Find the devices and save them. Cloud: account device list. Local: heartbeats.
    Discover(DiscoverArgs),
    /// Ask for fresh readings and print the state of all devices.
    Status(Link),
    /// Control an air purifier (HPH).
    #[command(subcommand)]
    Purifier(PurifierCommand),
    /// Control a smart socket (HSP).
    #[command(subcommand)]
    Socket(SocketCommand),
    /// Run the MCP server for AI assistants.
    Mcp(McpArgs),
}

/// The `--json` flag.
#[derive(Debug, Clone, Args)]
pub struct Json {
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

/// Options to reach the devices.
#[derive(Debug, Clone, Args)]
pub struct Link {
    /// Seconds to wait for the device.
    #[arg(long, default_value_t = 5.0, value_parser = parse_seconds)]
    timeout: f64,
    /// Control path. Default: config.
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[command(flatten)]
    output: Json,
}

/// Options to reach one device.
#[derive(Debug, Clone, Args)]
pub struct Target {
    /// Name, MAC or UUID.
    #[arg(short, long)]
    device: Option<String>,
    #[command(flatten)]
    link: Link,
}

/// Arguments of `login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Account e-mail address.
    #[arg(long)]
    email: Option<String>,
    /// Account phone number.
    #[arg(long)]
    phone: Option<String>,
    /// Country code for --phone.
    #[arg(long, default_value = "+91")]
    isd: String,
    #[command(flatten)]
    output: Json,
}

/// Arguments of `serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Log more.
    #[arg(long)]
    verbose: bool,
}

/// Arguments of `discover`.
#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Local path: seconds to listen.
    #[arg(long, default_value_t = 30.0, value_parser = parse_seconds)]
    seconds: f64,
    /// Control path. Default: config.
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[command(flatten)]
    output: Json,
}

/// Commands of `service`.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Start 'qubo serve' at boot. On Windows, also open TCP 8883 for the local network.
    Install(Json),
    /// Remove the boot start and the firewall rule.
    Uninstall(Json),
    /// Show if the boot start is installed.
    Status(Json),
}

/// A switch position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    /// On.
    On,
    /// Off.
    Off,
}

/// Commands of `purifier`.
#[derive(Debug, Subcommand)]
pub enum PurifierCommand {
    /// Switch the purifier on or off.
    Power {
        /// New state.
        #[arg(value_enum)]
        state: OnOff,
        /// Device and connection options.
        #[command(flatten)]
        target: Target,
    },
    /// Set the fan speed: 1 (low), 2 (medium), 3 (high).
    Speed {
        /// Fan speed.
        #[arg(value_parser = clap::value_parser!(u8).range(1..=3))]
        level: u8,
        /// Device and connection options.
        #[command(flatten)]
        target: Target,
    },
    /// Set the fan mode: auto or manual.
    Mode {
        /// Fan mode.
        #[arg(value_enum)]
        fan_mode: FanMode,
        /// Device and connection options.
        #[command(flatten)]
        target: Target,
    },
}

/// Commands of `socket`.
#[derive(Debug, Subcommand)]
pub enum SocketCommand {
    /// Switch the socket on or off. Switch-off above 15 W needs --force.
    Power {
        /// New state.
        #[arg(value_enum)]
        state: OnOff,
        /// Switch off above the 15 W limit.
        #[arg(long)]
        force: bool,
        /// Device and connection options.
        #[command(flatten)]
        target: Target,
    },
    /// Print power (W), voltage (V), current (A) and energy (kWh).
    Energy(Target),
}

/// How the MCP server talks to its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// Standard input and output.
    Stdio,
    /// Streamable HTTP.
    StreamableHttp,
}

/// Arguments of `mcp`.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Transport of the server.
    #[arg(long, value_enum, default_value = "stdio")]
    transport: Transport,
    /// Address for streamable-http.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port for streamable-http.
    #[arg(long, default_value_t = 8000)]
    port: u16,
}

fn parse_seconds(text: &str) -> std::result::Result<f64, String> {
    let seconds: f64 = text.parse().map_err(|_| format!("'{text}' is not a number"))?;
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(seconds)
    } else {
        Err("must be zero or more".to_owned())
    }
}

fn wait(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds)
}

/// Print rows as a table, or as JSON.
fn emit(data: &Value, as_json: bool) {
    if as_json {
        println!("{}", dumps(data));
        return;
    }
    let rows: Vec<&Map<String, Value>> = match data {
        Value::Array(items) => items.iter().filter_map(Value::as_object).collect(),
        Value::Object(row) => vec![row],
        _ => Vec::new(),
    };
    // Purifier and socket rows have different keys, so use the union of all keys.
    let mut columns: Vec<&str> = Vec::new();
    for key in rows.iter().flat_map(|row| row.keys()) {
        if !columns.contains(&key.as_str()) {
            columns.push(key);
        }
    }
    if columns.is_empty() {
        columns.push("result");
    }
    let mut table = Table::new();
    table.load_style(UTF8_FULL_CONDENSED);
    table.set_header(columns.iter().copied());
    for row in rows {
        table.add_row(columns.iter().map(|column| match row.get(*column) {
            None | Some(Value::Null) => "-".to_owned(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        }));
    }
    println!("{table}");
}

fn fail(error: &Error, as_json: bool) -> ExitCode {
    if as_json {
        println!("{}", dumps(&json!({ "error": error.to_string() })));
    } else {
        eprintln!("Error: {error}");
    }
    ExitCode::FAILURE
}

fn init_logging(verbose: bool) {
    let default = if verbose { "info" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // Logs go to stderr. Stdout belongs to the output, and to the MCP stdio transport.
    let _ =
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

/// Connect, run an action on one device, print the result and disconnect.
async fn with_device(
    paths: &Paths,
    kind: DeviceKind,
    target: &Target,
    action: impl AsyncFnOnce(&QuboClient, &Device) -> Result<Map<String, Value>>,
) -> Result<ExitCode> {
    let config = Config::load(paths)?;
    let client = connect(paths, &config, target.link.mode, wait(target.link.timeout)).await?;
    let result: Result<Map<String, Value>> = async {
        let devices = client.devices();
        let device = find_device(&devices, kind, target.device.as_deref())?;
        let mut row = Map::new();
        row.insert("device".to_owned(), device.label().into());
        row.insert("path".to_owned(), client.path().as_str().into());
        row.extend(action(&client, device).await?);
        Ok(row)
    }
    .await;
    client.stop().await;
    emit(&Value::Object(result?), target.link.output.json);
    Ok(ExitCode::SUCCESS)
}

fn confirmed(sent: bool) -> Map<String, Value> {
    Map::from_iter([("confirmed".to_owned(), Value::Bool(sent))])
}

async fn read_line(prompt: &'static str) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_owned())
    })
    .await
    .map_err(|err| Error::Value(err.to_string()))?
}

async fn login(paths: &Paths, args: &LoginArgs) -> Result<ExitCode> {
    if args.email.is_some() == args.phone.is_some() {
        return Err(Error::Value("Give --email or --phone.".to_owned()));
    }
    let session = CloudSession::load(paths)?;
    let logged_in = async {
        match (&args.email, &args.phone) {
            (Some(email), _) => {
                let password =
                    tokio::task::spawn_blocking(|| rpassword::prompt_password("Password: "))
                        .await
                        .map_err(|err| Error::Value(err.to_string()))??;
                session.login_email(email, &password).await?;
            }
            (_, Some(phone)) => {
                session.request_otp(phone, &args.isd).await?;
                let code = read_line("SMS code: ").await?;
                session.verify_otp(phone, &code, &args.isd).await?;
            }
            _ => {}
        }
        session.devices().await
    }
    .await;
    let devices = logged_in.map_err(|err| Error::Auth(format!("Login failed: {err}")))?;
    let mut config = Config::load(paths)?;
    config.merge(devices.iter().cloned());
    config.save(paths)?;
    let rows: Vec<Value> =
        devices.iter().map(|d| json!({ "device": d.label(), "kind": d.kind.name() })).collect();
    emit(&Value::Array(rows), args.output.json);
    Ok(ExitCode::SUCCESS)
}

async fn status(paths: &Paths, link: &Link) -> Result<ExitCode> {
    let config = Config::load(paths)?;
    let client = connect(paths, &config, link.mode, wait(link.timeout)).await?;
    let result = async {
        if client.devices().is_empty() {
            return Err(Error::Lookup(
                "No devices. Run 'qubo login' or 'qubo discover'.".to_owned(),
            ));
        }
        client.refresh(wait(link.timeout)).await?;
        Ok(client.telemetry())
    }
    .await;
    client.stop().await;
    let rows = result?.into_iter().map(Value::Object).collect();
    emit(&Value::Array(rows), link.output.json);
    Ok(ExitCode::SUCCESS)
}

async fn discover(paths: &Paths, args: &DiscoverArgs) -> Result<ExitCode> {
    let mut config = Config::load(paths)?;
    let client = connect(paths, &config, args.mode, Duration::from_secs(5)).await?;
    if client.path() == ControlPath::Local {
        tokio::time::sleep(wait(args.seconds)).await;
    }
    let found = client.devices();
    client.stop().await;
    config.merge(found);
    let saved = config.save(paths)?;
    if !args.output.json {
        println!("Saved {} devices to {}", config.devices.len(), saved.display());
    }
    emit(&serde_json::to_value(&config.devices)?, args.output.json);
    Ok(ExitCode::SUCCESS)
}

async fn purifier(paths: &Paths, command: &PurifierCommand) -> Result<ExitCode> {
    match command {
        PurifierCommand::Power { state, target } => {
            let command = |d: &Device| protocol::power(d, *state == OnOff::On);
            send_to(paths, DeviceKind::Purifier, target, command).await
        }
        PurifierCommand::Speed { level, target } => {
            let level = i64::from(*level);
            with_device(paths, DeviceKind::Purifier, target, async |client, device| {
                let command = protocol::fan_speed(device, level)?;
                Ok(confirmed(client.send(&command, wait(target.link.timeout)).await?))
            })
            .await
        }
        PurifierCommand::Mode { fan_mode, target } => {
            send_to(paths, DeviceKind::Purifier, target, |d| protocol::fan_mode(d, *fan_mode)).await
        }
    }
}

async fn send_to(
    paths: &Paths,
    kind: DeviceKind,
    target: &Target,
    command: impl FnOnce(&Device) -> protocol::Command,
) -> Result<ExitCode> {
    with_device(paths, kind, target, async |client, device| {
        Ok(confirmed(client.send(&command(device), wait(target.link.timeout)).await?))
    })
    .await
}

async fn socket(paths: &Paths, command: &SocketCommand) -> Result<ExitCode> {
    match command {
        SocketCommand::Power { state, force, target } => {
            with_device(paths, DeviceKind::Socket, target, async |client, device| {
                let on = *state == OnOff::On;
                let sent =
                    client.switch_socket(device, on, *force, wait(target.link.timeout)).await?;
                Ok(confirmed(sent))
            })
            .await
        }
        SocketCommand::Energy(target) => {
            with_device(paths, DeviceKind::Socket, target, async |client, device| {
                let refresh = protocol::metering_refresh(device, 60);
                let fresh = client.send(&refresh, wait(target.link.timeout)).await?;
                let mut row = Map::from_iter([("fresh".to_owned(), Value::Bool(fresh))]);
                let values = client.reported(device, Service::Metering);
                if let Value::Object(energy) = serde_json::to_value(protocol::metering(&values))? {
                    row.extend(energy);
                }
                Ok(row)
            })
            .await
        }
    }
}

fn simple(result: &str, as_json: bool) -> ExitCode {
    emit(&json!({ "result": result }), as_json);
    ExitCode::SUCCESS
}

async fn dispatch(paths: &Paths, command: &Command) -> Result<ExitCode> {
    match command {
        Command::Login(args) => login(paths, args).await,
        Command::Logout(out) => {
            CloudSession::load(paths)?.logout()?;
            Ok(simple("logged out", out.json))
        }
        Command::Serve(args) => {
            init_logging(args.verbose);
            let config = Config::load(paths)?;
            println!(
                "qubo serve: devices on TLS {DEVICE_PORT}, local tools on 127.0.0.1:{}. Ctrl+C stops.",
                config.port
            );
            let stop = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            serve::serve(paths, config, BrokerConfig::default(), stop).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Service(ServiceCommand::Install(out)) => {
            service::install().map_err(|e| Error::Value(format!("Service install failed: {e}")))?;
            let mut row = json!({ "result": "installed" });
            if let Some(note) = service::Platform::current().ok().and_then(service::firewall_note) {
                row["note"] = note.into();
            }
            emit(&row, out.json);
            Ok(ExitCode::SUCCESS)
        }
        Command::Service(ServiceCommand::Uninstall(out)) => {
            service::uninstall()?;
            Ok(simple("uninstalled", out.json))
        }
        Command::Service(ServiceCommand::Status(out)) => {
            emit(&json!({ "installed": service::status()? }), out.json);
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor(out) => {
            let config = Config::load(paths)?;
            let rows = doctor::checks(paths, &config).await;
            emit(&serde_json::to_value(&rows)?, out.json);
            Ok(if rows.last().is_some_and(|row| row.ok) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Discover(args) => discover(paths, args).await,
        Command::Status(link) => status(paths, link).await,
        Command::Purifier(command) => purifier(paths, command).await,
        Command::Socket(command) => socket(paths, command).await,
        Command::Mcp(args) => {
            let server = mcp_server::QuboServer::new(paths.clone());
            match args.transport {
                Transport::Stdio => mcp_server::run_stdio(server).await?,
                Transport::StreamableHttp => {
                    mcp_server::run_http(server, &args.host, args.port).await?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

const fn wants_json(command: &Command) -> bool {
    match command {
        Command::Logout(out)
        | Command::Doctor(out)
        | Command::Service(
            ServiceCommand::Install(out)
            | ServiceCommand::Uninstall(out)
            | ServiceCommand::Status(out),
        ) => out.json,
        Command::Login(args) => args.output.json,
        Command::Discover(args) => args.output.json,
        Command::Status(link) => link.output.json,
        Command::Purifier(
            PurifierCommand::Power { target, .. }
            | PurifierCommand::Speed { target, .. }
            | PurifierCommand::Mode { target, .. },
        )
        | Command::Socket(SocketCommand::Power { target, .. } | SocketCommand::Energy(target)) => {
            target.link.output.json
        }
        Command::Serve(_) | Command::Mcp(_) => false,
    }
}

/// Run a parsed command line. Return the exit code of the process.
pub async fn run(cli: Cli, paths: &Paths) -> ExitCode {
    init_logging(false);
    match dispatch(paths, &cli.command).await {
        Ok(code) => code,
        Err(error) => fail(&error, wants_json(&cli.command)),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    mod cli {
        use super::*;

        #[test]
        fn has_consistent_command_definitions() {
            Cli::command().debug_assert();
        }

        #[test]
        fn accepts_a_purifier_speed_of_3() {
            assert!(Cli::try_parse_from(["qubo", "purifier", "speed", "3"]).is_ok());
        }

        #[test]
        fn refuses_a_purifier_speed_of_4() {
            assert!(Cli::try_parse_from(["qubo", "purifier", "speed", "4"]).is_err());
        }

        #[test]
        fn refuses_a_negative_timeout() {
            assert!(Cli::try_parse_from(["qubo", "status", "--timeout", "-1"]).is_err());
        }

        #[test]
        fn accepts_a_fractional_timeout() {
            assert!(Cli::try_parse_from(["qubo", "status", "--timeout", "0.5"]).is_ok());
        }
    }

    mod wants_json {
        use super::*;

        #[test]
        fn is_true_for_a_nested_command_with_the_flag() {
            let cli = Cli::try_parse_from(["qubo", "socket", "energy", "--json"]).unwrap();
            assert!(wants_json(&cli.command));
        }

        #[test]
        fn is_false_without_the_flag() {
            let cli = Cli::try_parse_from(["qubo", "status"]).unwrap();
            assert!(!wants_json(&cli.command));
        }
    }

    mod parse_seconds {
        use super::*;

        #[test]
        fn refuses_text() {
            assert!(parse_seconds("soon").is_err());
        }

        #[test]
        fn refuses_infinity() {
            assert!(parse_seconds("inf").is_err());
        }
    }
}
