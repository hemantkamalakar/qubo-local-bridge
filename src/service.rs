//! `qubo service`: start `qubo serve` at boot, and the Windows firewall rule.

use std::path::PathBuf;
use std::process::Command;

use crate::error::{Error, Result};

const TASK: &str = "qubo-serve";
const RULE: &str = "qubo-local-bridge-mqtt-tls";
/// Name of the systemd user unit.
pub const UNIT: &str = "qubo-serve.service";

/// The operating systems that `qubo service` supports.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows. A scheduled task starts the broker at logon.
    Windows,
    /// Linux. A systemd user unit starts the broker at boot.
    Linux,
}

impl Platform {
    /// The platform of this build.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Value`] if this operating system is not Windows or Linux.
    pub fn current() -> Result<Self> {
        Self::from_os(std::env::consts::OS)
    }

    /// The platform for an operating system name, as in `std::env::consts::OS`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Value`] if the operating system is not Windows or Linux.
    ///
    /// # Examples
    ///
    /// ```
    /// use qubo_bridge::service::Platform;
    ///
    /// assert_eq!(Platform::from_os("linux").ok(), Some(Platform::Linux));
    /// assert!(Platform::from_os("macos").is_err());
    /// ```
    pub fn from_os(os: &str) -> Result<Self> {
        match os {
            "windows" => Ok(Self::Windows),
            "linux" => Ok(Self::Linux),
            other => Err(Error::Value(format!(
                "'qubo service' supports Windows and Linux, not {other}."
            ))),
        }
    }
}

/// A security note for the device port, or `None` if `qubo service install` handles it.
///
/// The broker accepts devices without a login. Windows gets a firewall rule for the local
/// network. Linux gets no rule, so the user must add one.
///
/// # Examples
///
/// ```
/// use qubo_bridge::service::{Platform, firewall_note};
///
/// assert!(firewall_note(Platform::Linux).is_some());
/// assert!(firewall_note(Platform::Windows).is_none());
/// ```
#[must_use]
pub const fn firewall_note(platform: Platform) -> Option<&'static str> {
    match platform {
        Platform::Windows => None,
        Platform::Linux => Some(
            "Port 8883 has no login and no firewall rule. Allow TCP 8883 from the local network only, for example: sudo ufw allow from 192.168.0.0/16 to any port 8883 proto tcp",
        ),
    }
}

type Step = Vec<String>;

fn step<const N: usize>(parts: [&str; N]) -> Step {
    parts.iter().map(|p| (*p).to_owned()).collect()
}

/// Quote one argument for a POSIX shell.
///
/// # Examples
///
/// ```
/// use qubo_bridge::service::shell_quote;
///
/// assert_eq!(shell_quote("/usr/bin/qubo"), "/usr/bin/qubo");
/// assert_eq!(shell_quote("/opt/my tools/qubo"), "'/opt/my tools/qubo'");
/// ```
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r#"'"'"'"#))
    }
}

/// Quote one argument for the Windows command line.
fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The command that starts this program.
///
/// # Errors
///
/// Returns [`Error::Io`] if the path of this program is not known.
pub fn qubo_command() -> Result<Vec<String>> {
    Ok(vec![std::env::current_exe()?.to_string_lossy().into_owned()])
}

fn unit_path() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".config").join("systemd").join("user").join(UNIT)
}

/// The text of the systemd unit.
///
/// # Examples
///
/// ```
/// use qubo_bridge::service::unit_file;
///
/// let unit = unit_file(&["/usr/bin/qubo".to_owned()]);
/// assert!(unit.contains("ExecStart=/usr/bin/qubo serve"));
/// ```
#[must_use]
pub fn unit_file(command: &[String]) -> String {
    let exec: Vec<String> = command.iter().map(|a| shell_quote(a)).collect();
    format!(
        "[Unit]\nDescription=qubo-local-bridge local broker\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

fn elevated(arguments: &str) -> Step {
    // One administrator prompt (UAC) for the firewall change.
    step([
        "powershell",
        "-NoProfile",
        "-Command",
        &format!("Start-Process netsh -Verb RunAs -Wait -ArgumentList '{arguments}'"),
    ])
}

/// The commands that install the boot start.
#[must_use]
pub fn install_commands(platform: Platform, command: &[String]) -> Vec<Step> {
    match platform {
        Platform::Windows => {
            let mut line: Vec<String> = command.iter().map(|a| windows_quote(a)).collect();
            line.push("serve".to_owned());
            // The task opens a console window at logon. Use a Windows service if that is a problem.
            vec![
                step([
                    "schtasks",
                    "/Create",
                    "/TN",
                    TASK,
                    "/SC",
                    "ONLOGON",
                    "/RL",
                    "LIMITED",
                    "/F",
                    "/TR",
                    &line.join(" "),
                ]),
                elevated(&format!(
                    "advfirewall firewall add rule name={RULE} dir=in action=allow protocol=TCP \
                     localport=8883 remoteip=localsubnet profile=any"
                )),
            ]
        }
        Platform::Linux => vec![
            step(["systemctl", "--user", "daemon-reload"]),
            step(["systemctl", "--user", "enable", "--now", UNIT]),
            vec!["loginctl".to_owned(), "enable-linger".to_owned(), user_name()],
        ],
    }
}

/// The commands that remove the boot start.
#[must_use]
pub fn uninstall_commands(platform: Platform) -> Vec<Step> {
    match platform {
        Platform::Windows => vec![
            step(["schtasks", "/Delete", "/TN", TASK, "/F"]),
            elevated(&format!("advfirewall firewall delete rule name={RULE}")),
        ],
        Platform::Linux => vec![step(["systemctl", "--user", "disable", "--now", UNIT])],
    }
}

/// The command that tells whether the boot start is installed.
#[must_use]
pub fn status_command(platform: Platform) -> Step {
    match platform {
        Platform::Windows => step(["schtasks", "/Query", "/TN", TASK]),
        Platform::Linux => step(["systemctl", "--user", "is-active", UNIT]),
    }
}

fn user_name() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .unwrap_or_default()
}

fn run(command: &[String]) -> Result<std::process::ExitStatus> {
    let (program, args) =
        command.split_first().ok_or_else(|| Error::Value("Empty command.".to_owned()))?;
    Ok(Command::new(program).args(args).status()?)
}

/// Install the boot start.
///
/// # Errors
///
/// Returns [`Error::Value`] if a step fails or the platform is not supported, and [`Error::Io`] if a file or process fails.
pub fn install() -> Result<()> {
    let platform = Platform::current()?;
    let command = qubo_command()?;
    if platform == Platform::Linux {
        let path = unit_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, unit_file(&command))?;
    }
    for step in install_commands(platform, &command) {
        let status = run(&step)?;
        if !status.success() {
            return Err(Error::Value(format!("'{}' failed with {status}.", step.join(" "))));
        }
    }
    Ok(())
}

/// Remove the boot start.
///
/// # Errors
///
/// Returns [`Error::Value`] if the platform is not supported.
pub fn uninstall() -> Result<()> {
    let platform = Platform::current()?;
    for step in uninstall_commands(platform) {
        // Remove what exists. Ignore what does not.
        let _ = run(&step);
    }
    if platform == Platform::Linux {
        let _ = std::fs::remove_file(unit_path());
    }
    Ok(())
}

/// Whether the boot start is installed.
///
/// # Errors
///
/// Returns [`Error::Value`] if the platform is not supported.
pub fn status() -> Result<bool> {
    let command = status_command(Platform::current()?);
    let (program, args) =
        command.split_first().ok_or_else(|| Error::Value("Empty command.".to_owned()))?;
    Ok(Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_steps() -> Vec<Step> {
        install_commands(Platform::Windows, &["C:/x/qubo.exe".to_owned()])
    }

    mod install_commands_on_windows {
        use super::*;

        #[test]
        fn creates_a_scheduled_task_first() {
            assert_eq!(windows_steps()[0][..3], ["schtasks", "/Create", "/TN"]);
        }

        #[test]
        fn runs_serve_in_the_task() {
            assert_eq!(windows_steps()[0].last().map(String::as_str), Some("C:/x/qubo.exe serve"));
        }

        #[test]
        fn opens_port_8883_for_the_local_network() {
            let rule = windows_steps()[1].last().cloned().unwrap_or_default();
            assert!(rule.contains("localport=8883 remoteip=localsubnet"), "{rule}");
        }

        #[test]
        fn quotes_a_program_path_with_spaces() {
            let steps = install_commands(Platform::Windows, &["C:/My Tools/qubo.exe".to_owned()]);
            assert_eq!(
                steps[0].last().map(String::as_str),
                Some(r#""C:/My Tools/qubo.exe" serve"#)
            );
        }
    }

    mod uninstall_commands_on_windows {
        use super::*;

        #[test]
        fn deletes_the_scheduled_task_first() {
            assert_eq!(uninstall_commands(Platform::Windows)[0][..2], ["schtasks", "/Delete"]);
        }
    }

    mod install_commands_on_linux {
        use super::*;

        #[test]
        fn enables_and_starts_the_unit() {
            let steps = install_commands(Platform::Linux, &["/home/u/.local/bin/qubo".to_owned()]);
            assert!(steps.contains(&step(["systemctl", "--user", "enable", "--now", UNIT])));
        }
    }

    mod unit_file {
        use super::*;

        #[test]
        fn quotes_a_program_path_with_spaces() {
            let unit = unit_file(&["/opt/my tools/qubo".to_owned()]);
            assert!(unit.contains("ExecStart='/opt/my tools/qubo' serve"), "{unit}");
        }

        #[test]
        fn restarts_the_service_on_failure() {
            assert!(unit_file(&["/usr/bin/qubo".to_owned()]).contains("Restart=on-failure"));
        }
    }

    mod firewall_note {
        use super::*;

        #[test]
        fn is_empty_on_windows_because_install_adds_the_rule() {
            assert_eq!(firewall_note(Platform::Windows), None);
        }

        #[test]
        fn names_port_8883_on_linux() {
            assert!(firewall_note(Platform::Linux).is_some_and(|n| n.contains("8883")));
        }
    }

    mod platform {
        use super::*;

        #[test]
        fn refuses_an_unsupported_operating_system() {
            assert!(Platform::from_os("macos").is_err());
        }

        #[test]
        fn accepts_linux() {
            assert_eq!(Platform::from_os("linux").ok(), Some(Platform::Linux));
        }

        #[test]
        fn accepts_windows() {
            assert_eq!(Platform::from_os("windows").ok(), Some(Platform::Windows));
        }
    }

    mod shell_quote {
        use super::*;

        #[test]
        fn keeps_a_safe_word_as_it_is() {
            assert_eq!(shell_quote("/usr/bin/qubo"), "/usr/bin/qubo");
        }

        #[test]
        fn quotes_an_empty_argument() {
            assert_eq!(shell_quote(""), "''");
        }

        #[test]
        fn escapes_a_single_quote() {
            assert_eq!(shell_quote("it's"), r#"'it'"'"'s'"#);
        }
    }
}
