// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for VSCode editors.

use anyhow::{Context, Result};
use logcontrol_tracing::{PrettyLogControl1LayerFactory, TracingLogControl1};
use logcontrol_zbus::{ConnectionBuilderExt, LogControl1};
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

use crate::providers::PROVIDERS;
use crate::searchprovider::{App, AppId, VSCodeWorkspaceSearchProvider};
use crate::storage::GlobalStorage;

mod providers;
mod searchprovider;
mod storage;
mod systemd;

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.VSCode";

async fn tick(connection: zbus::Connection) {
    loop {
        connection.executor().tick().await
    }
}

fn app() -> clap::Command {
    use clap::*;
    command!()
        .dont_collapse_args_in_usage(true)
        .term_width(80)
        .after_help(
            "\
Set $RUST_LOG to control the log level",
        )
        .arg(
            Arg::new("providers")
                .long("providers")
                .action(ArgAction::SetTrue)
                .help("List all providers"),
        )
}

fn main() -> Result<()> {
    let matches = app().get_matches();
    if matches.get_flag("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{label}")
        }
        Ok(())
    } else {
        // Setup env filter for convenient log control on console
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().ok();
        // If an env filter is set with $RUST_LOG use the lowest level as default for the control part,
        // to make sure the env filter takes precedence initially.
        let default_level = if env_filter.is_some() {
            Level::TRACE
        } else {
            Level::INFO
        };
        let (control, control_layer) =
            TracingLogControl1::new_auto(PrettyLogControl1LayerFactory, default_level)
                .with_context(|| "Failed to setup logging".to_string())?;

        // Setup tracing: If we're connected to systemd, directly log to the journal, otherwise log nicely to the TTY.
        tracing::subscriber::set_global_default(
            Registry::default().with(env_filter).with(control_layer),
        )
        .unwrap();
        // Direct glib to rust log, and…
        glib::log_set_default_handler(glib::rust_log_handler);
        // …rust log to tracing.
        tracing_log::LogTracer::init().unwrap();

        event!(
            Level::INFO,
            "Started {} version: {}",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION")
        );

        event!(
            Level::DEBUG,
            "Connecting to session bus, registering interfaces for search providers, and acquiring {}",
            BUSNAME
        );

        // Connect to DBus and register all our objects for search providers.
        let user_config_dir = glib::user_config_dir();
        let connection = glib::MainContext::default().block_on(async {
            PROVIDERS
                .iter()
                .filter_map(|provider| {
                    gio::DesktopAppInfo::new(provider.desktop_id).map(|gio_app| {
                        event!(Level::INFO, "Found app {}", provider.desktop_id);
                        (gio_app, provider)
                    })
                })
                .try_fold(
                    // We disable the internal executor because we'd like to run the connection
                    // exclusively on the glib mainloop, and thus tick it manually (see below).
                    zbus::ConnectionBuilder::session()?.internal_executor(false),
                    |builder, (gio_app, provider)| {
                        let config_dir = user_config_dir.join(provider.config.dirname);
                        let storage = GlobalStorage::open_from_config_directory(&config_dir)
                            .with_context(|| {
                                format!(
                                    "Failed to open global storage database in config directory {}",
                                    config_dir.display()
                                )
                            })?;
                        let app_id = AppId::from(&gio_app);
                        let search_provider =
                            VSCodeWorkspaceSearchProvider::new(App::from(gio_app), storage);
                        let path = provider.objpath();
                        event!(
                            Level::DEBUG,
                            app_id = %app_id,
                            "Serving search provider for {} at {}",
                            search_provider.app().id(),
                            &path
                        );
                        builder.serve_at(path, search_provider).with_context(|| {
                            format!(
                                "Failed to serve search provider for {} at {}",
                                app_id,
                                provider.objpath()
                            )
                        })
                    },
                )?
                .serve_log_control(LogControl1::new(control))?
                .name(BUSNAME)?
                .build()
                .await
                .with_context(|| "Failed to connect to session bus")
        })?;

        // Manually tick the connection on the glib mainloop to make all code in zbus run on the mainloop.
        glib::MainContext::default().spawn(tick(connection.clone()));

        event!(
            Level::INFO,
            "Acquired name {}, serving search providers",
            BUSNAME
        );

        let mainloop = glib::MainLoop::new(None, false);

        // Quit our mainloop on SIGTERM and SIGINT
        glib::source::unix_signal_add(
            libc::SIGTERM,
            glib::clone!(@strong mainloop =>  move || {
                event!(Level::DEBUG, "Terminated, quitting mainloop");
                mainloop.quit();
                glib::ControlFlow::Break
            }),
        );
        glib::source::unix_signal_add(
            libc::SIGINT,
            glib::clone!(@strong mainloop =>  move || {
                event!(Level::DEBUG, "Interrupted, quitting mainloop");
                mainloop.quit();
                glib::ControlFlow::Break
            }),
        );

        mainloop.run();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::app;

    #[test]
    fn verify_app() {
        app().debug_assert();
    }
}
