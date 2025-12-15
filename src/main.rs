// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// Licensed under the EUPL
//
// See https://interoperable-europe.ec.europa.eu/collection/eupl/eupl-text-eupl-12

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

use std::time::Duration;

use async_executor::LocalExecutor;
use async_io::Timer;
use async_signal::Signals;
use futures_lite::{StreamExt as _, future::race, stream};
use logcontrol_tracing::{PrettyLogControl1LayerFactory, TracingLogControl1};
use logcontrol_zbus::{ConnectionBuilderExt, logcontrol::LogControl1};
use searchprovider::{CodeVariant, SearchProvider};
use tracing::{Level, info, warn};
use tracing_subscriber::{Registry, layer::SubscriberExt};

mod search;
mod searchprovider;
mod workspaces;
mod xdg;

fn setup_logging() -> impl LogControl1 {
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
        TracingLogControl1::new_auto(PrettyLogControl1LayerFactory, default_level).unwrap();
    let subscriber = Registry::default().with(env_filter).with(control_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();
    control
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let logcontrol = setup_logging();
    tracing::info!(
        "Starting VSCode search providers for GNOME version {}",
        env!("CARGO_PKG_VERSION")
    );
    let executor = LocalExecutor::new().leak();

    let main_task = executor.spawn(async move {
        let connection = zbus::connection::Builder::session()?
            .name("de.swsnr.VSCodeSearchProvider")?
            .internal_executor(false)
            .serve_log_control(logcontrol_zbus::LogControl1::new(logcontrol))?
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
        let terminate = Signals::new([async_signal::Signal::Term, async_signal::Signal::Int])?
            .filter_map(|signal| {
                signal
                    .inspect_err(|error| {
                        warn!("Signal failed: {error}");
                    })
                    .ok()
            })
            .inspect(|signal| {
                info!("Received termination signal {signal:?}, terminating");
            })
            .map(|_| Err(()))
            .race(
                stream::repeat(())
                    .then(|()| {
                        race(
                            async {
                                connection.monitor_activity().await;
                                Ok(())
                            },
                            async {
                                let timeout = Duration::from_secs(300);
                                Timer::after(timeout).await;
                                info!("Connection idle for {timeout:#?}, terminating");
                                Err(())
                            },
                        )
                    })
                    .filter(Result::is_err),
            )
            .take(1)
            .last();

        stream::stop_after_future(
            stream::repeat(()).then(|()| connection.executor().tick()),
            terminate,
        )
        .last()
        .await;

        connection.graceful_shutdown().await;
        info!("Exiting");
        Ok(())
    });

    async_io::block_on(executor.run(main_task))
}
