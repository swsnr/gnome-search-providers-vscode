// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, clippy::all, clippy::pedantic,
    // Guard against left-over debugging output
    clippy::dbg_macro,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::unimplemented,
    clippy::use_debug,
    clippy::todo,
    // We must use tokio's APIs to exit the app.
    clippy::exit,
    // Do not carelessly ignore errors
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
)]
#![allow(clippy::used_underscore_binding)]
#![forbid(unsafe_code)]

use futures_util::FutureExt;
use logcontrol_tracing::{PrettyLogControl1LayerFactory, TracingLogControl1};
use logcontrol_zbus::ConnectionBuilderExt;
use searchprovider::{CodeVariant, SearchProvider};
use tokio::{
    signal::{
        ctrl_c,
        unix::{SignalKind, signal},
    },
    time::{Duration, Instant},
};
use tracing::{Level, error, info};
use tracing_subscriber::{Registry, layer::SubscriberExt};

mod search;
mod searchprovider;
mod workspaces;
mod xdg;

/// Return when `connection` was idle for the `idle_timeout` duration.
async fn connection_idle_timeout(connection: &zbus::Connection, idle_timeout: Duration) {
    let idle_timer = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_timer);
    loop {
        tokio::select! {
            () = connection.monitor_activity() => {
                idle_timer.as_mut().reset(Instant::now() + idle_timeout);
            }
            () = &mut idle_timer => {
                break;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup env filter for convenient log control on console
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().ok();
    // If an env filter is set with $RUST_LOG use the lowest level as default for the control part,
    // to make sure the env filter takes precedence initially.
    let default_level = if env_filter.is_some() {
        Level::TRACE
    } else if cfg!(debug_assertions) {
        // In debug builds, e.g. local testing, log more by default
        Level::DEBUG
    } else {
        Level::INFO
    };
    let (control, control_layer) =
        TracingLogControl1::new_auto(PrettyLogControl1LayerFactory, default_level)?;
    let subscriber = Registry::default().with(env_filter).with(control_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    tracing::info!(
        "Starting VSCode search providers for GNOME version {}",
        env!("CARGO_PKG_VERSION")
    );

    let connection = zbus::connection::Builder::session()?
        .name("de.swsnr.VSCodeSearchProvider")?
        .serve_log_control(logcontrol_zbus::LogControl1::new(control))?
        .serve_at(
            "/de/swsnr/VSCodeSearchProvider/code_oss",
            SearchProvider::new(CodeVariant {
                app_id: "code-oss",
                config_directory_name: "Code - OSS",
            }),
        )?
        .serve_at(
            "/de/swsnr/VSCodeSearchProvider/code",
            SearchProvider::new(CodeVariant {
                app_id: "code",
                config_directory_name: "Code",
            }),
        )?
        .serve_at(
            "/de/swsnr/VSCodeSearchProvider/codium",
            SearchProvider::new(CodeVariant {
                app_id: "codium",
                config_directory_name: "VSCodium",
            }),
        )?
        .serve_at(
            "/de/swsnr/VSCodeSearchProvider/code_insiders",
            SearchProvider::new(CodeVariant {
                app_id: "code-insiders",
                config_directory_name: "Code - Insiders",
            }),
        )?
        .build()
        .await?;
    info!("Connected to bus, serving search provider");

    // Exit the service on Ctrl+C (i.e. keyboard interrupt on the local console),
    // SIGTERM, i.e. from systemd, and when it's been idle for a while so
    // that it doesn't keep running even if the user doesn't search anymore.
    let idle_timeout = Duration::from_secs(300);
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        () = connection_idle_timeout(&connection, idle_timeout) => {
            info!("Idle timeout after {idle_timeout:?}");
            // We know that there's not activity on the connection at this point
            // so we forcibly close it fast.
            connection.close().await?;
        }
        result = ctrl_c().fuse() => {
            if let Err(error) = result {
                error!("Ctrl-C failed? {error}");
            } else {
                info!("Received SIGINT");
            }
            connection.graceful_shutdown().await;
        }
        _ = sigterm.recv().fuse() => {
            info!("Received SIGTERM");
            connection.graceful_shutdown().await;
        }
    }

    info!("Exiting");
    Ok(())
}
