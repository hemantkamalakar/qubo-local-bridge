//! Select the control path: the local broker of `qubo serve` or the QUBO cloud.

use std::time::Duration;

use crate::client::{ClientConfig, QuboClient};
use crate::cloud::CloudSession;
use crate::error::{Error, Result};
use crate::models::{Config, Mode, Paths};

const LOCAL_PROBE: Duration = Duration::from_secs(2);

/// Pick a path. This function holds the selection rules only.
///
/// - `local`: use the local broker. A failure is final.
/// - `cloud`: use the cloud.
/// - `auto`: use the local broker if it accepts the connection, else the cloud.
///
/// # Errors
///
/// Returns the error of the path that was tried last. In `auto` mode, a [`Error::Connection`] error from the local path moves on to the cloud path.
///
/// # Examples
///
/// ```
/// # use qubo_bridge::connect::select;
/// # use qubo_bridge::error::Error;
/// # use qubo_bridge::models::Mode;
/// # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
/// // The local path is down, so "auto" mode uses the cloud path.
/// let path = runtime.block_on(select(
///     Mode::Auto,
///     async || Err::<&str, _>(Error::Connection("down".to_owned())),
///     async || Ok("cloud"),
/// ))?;
/// assert_eq!(path, "cloud");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub async fn select<T>(
    mode: Mode,
    local: impl AsyncFnOnce() -> Result<T>,
    cloud: impl AsyncFnOnce() -> Result<T>,
) -> Result<T> {
    if mode != Mode::Cloud {
        match local().await {
            Ok(found) => return Ok(found),
            Err(Error::Connection(_)) if mode == Mode::Auto => {}
            Err(err) => return Err(err),
        }
    }
    cloud().await
}

async fn connect_cloud(paths: &Paths, config: &Config, timeout: Duration) -> Result<QuboClient> {
    let session = CloudSession::load(paths)?;
    if !session.logged_in().await {
        return Err(Error::NoPath(
            "No local broker on this host and no cloud login. Run 'qubo login' or 'qubo serve'."
                .to_owned(),
        ));
    }
    let devices =
        if config.devices.is_empty() { session.devices().await? } else { config.devices.clone() };
    let client_config = session.client_config(devices.clone(), Some(paths.state())).await;
    let client = QuboClient::connect(client_config, timeout + Duration::from_secs(5)).await?;
    for device in &devices {
        // The shadow is optional. Live reports still arrive without it.
        let Ok(shadow) = session.shadow(device).await else {
            continue;
        };
        for (service, values) in shadow {
            client.seed(device, &service, values);
        }
    }
    Ok(client)
}

/// Connect on the selected path. `mode` overrides the mode in the configuration.
///
/// # Errors
///
/// Returns [`Error::Auth`] if the token provider fails, and [`Error::Connection`] if the broker cannot be reached or does not accept the connection in time.
pub async fn connect(
    paths: &Paths,
    config: &Config,
    mode: Option<Mode>,
    timeout: Duration,
) -> Result<QuboClient> {
    let mode = mode.unwrap_or(config.mode);
    select(
        mode,
        async || {
            let wait = if mode == Mode::Local { timeout } else { LOCAL_PROBE };
            let local = ClientConfig::local(
                config.host.clone(),
                config.port,
                config.devices.clone(),
                Some(paths.state()),
            );
            QuboClient::connect(local, wait).await
        },
        async || connect_cloud(paths, config, timeout).await,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pick(mode: Mode, local_ok: bool, logged_in: bool) -> Result<&'static str> {
        select(
            mode,
            async || {
                if local_ok { Ok("local") } else { Err(Error::Connection("down".to_owned())) }
            },
            async || {
                if logged_in { Ok("cloud") } else { Err(Error::NoPath("none".to_owned())) }
            },
        )
        .await
    }

    mod select {
        use super::*;

        #[tokio::test]
        async fn auto_mode_uses_local_when_it_connects() {
            assert_eq!(pick(Mode::Auto, true, true).await.unwrap(), "local");
        }

        #[tokio::test]
        async fn auto_mode_falls_back_to_cloud_when_local_is_down() {
            assert_eq!(pick(Mode::Auto, false, true).await.unwrap(), "cloud");
        }

        #[tokio::test]
        async fn local_mode_returns_the_connection_error_when_local_is_down() {
            let err = pick(Mode::Local, false, true).await.unwrap_err();
            assert!(matches!(err, Error::Connection(_)));
        }

        #[tokio::test]
        async fn cloud_mode_skips_the_local_path() {
            assert_eq!(pick(Mode::Cloud, true, true).await.unwrap(), "cloud");
        }

        #[tokio::test]
        async fn auto_mode_returns_no_path_when_both_paths_fail() {
            let err = pick(Mode::Auto, false, false).await.unwrap_err();
            assert!(matches!(err, Error::NoPath(_)));
        }
    }
}
