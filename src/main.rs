// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for VSCode editors.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_executor::block_on_stream;
use gnome_search_provider_common::app::*;
use gnome_search_provider_common::futures_channel::{mpsc, oneshot};
use gnome_search_provider_common::futures_util::{SinkExt, StreamExt};
use gnome_search_provider_common::gio;
use gnome_search_provider_common::gio::glib;
use gnome_search_provider_common::logging::*;
use gnome_search_provider_common::mainloop::*;
use gnome_search_provider_common::matching::*;
use gnome_search_provider_common::zbus;
use tracing::{event, instrument, Level, Span};
use tracing_futures::Instrument;

use crate::providers::PROVIDERS;
use crate::storage::{GlobalStorage, StorageOpenedPathsList};

mod providers;
mod storage;

/// A recent workspace of a VSCode variant.
#[derive(Debug, PartialEq)]
struct RecentWorkspace {
    /// The human readable nfame.
    name: String,
    /// The workspace URL.
    url: String,
}

fn recent_item(url: String) -> Result<AppLaunchItem> {
    if let Some(name) = url.split('/').last() {
        let item = AppLaunchItem {
            name: name.to_string(),
            uri: url,
        };
        event!(Level::TRACE, "Found recent workspace item {:?}", item);
        Ok(item)
    } else {
        Err(anyhow!("Failed to extract workspace name from URL {}", url))
    }
}

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.VSCode";

async fn tick(connection: zbus::Connection) {
    loop {
        connection.executor().tick().await
    }
}

struct Service {
    app_launch_service: AppLaunchService,
    connection: zbus::Connection,
}

/// Handle a single search provider request.
///
/// Handle `request` and return the new list of app items, if any.
#[instrument(skip(items, storage_tx), fields(app_id=%app_id, request=%request.name()))]
async fn handle_search_provider_request(
    app_id: AppId,
    mut storage_tx: mpsc::Sender<(Span, oneshot::Sender<Result<StorageOpenedPathsList>>)>,
    items: Option<Arc<IndexMap<String, AppLaunchItem>>>,
    request: AppItemSearchRequest,
) -> Option<Arc<IndexMap<String, AppLaunchItem>>> {
    match request {
        AppItemSearchRequest::Invalidate(_) => {
            if items.is_some() {
                event!(Level::DEBUG, %app_id, "Invalidating cached projects");
            }
            None
        }
        AppItemSearchRequest::GetItems(_, respond_to) => {
            let reply = match items {
                None => {
                    let (tx, rx) = oneshot::channel();
                    if storage_tx.send((Span::current(), tx)).await.is_err() {
                        event!(Level::ERROR, %app_id, "Global storage thread no longer running");
                        Err(zbus::fdo::Error::Failed(
                            "Global storage thread no longer running".to_string(),
                        ))
                    } else {
                        rx.await
                            .map_err(|_| {
                                event!(Level::ERROR, %app_id, "Channel end dropped while waiting for response");
                                zbus::fdo::Error::Failed("Failed to get recent items".to_string())
                            })
                        .and_then(|result| {
                            result.map_err(|error| {
                                event!(Level::ERROR, %app_id, %error, "Failed to query recent items: {:#}", error);
                                zbus::fdo::Error::Failed(format!("Failed to query recent items: {error}"))
                            })
                        }).map(|list| {
                            let mut map = IndexMap::new();
                            let urls = list.into_workspace_urls();
                            for url in urls {
                                event!(Level::TRACE, %app_id, "Discovered workspace url {}", url);
                                let id = format!("vscode-search-provider-{}-{}", app_id, &url);
                                match recent_item(url) {
                                    Ok(item) => {
                                        map.insert(id, item);
                                    }
                                    Err(err) => {
                                        event!(Level::WARN, %app_id, "Skipping workspace: {}", err)
                                    }
                                }
                            }
                            Arc::new(map)
                        })
                    }
                }
                Some(ref items) => Ok(Arc::clone(items)),
            };
            let items = reply.as_ref().map(|a| a.clone()).ok();
            // We don't care if the receiver was dropped before we could answer it.
            let _ = respond_to.send(reply);
            items
        }
    }
}
/// Serve search provider requests.
///
/// Loop over requests received from `rx`, and provide the search provider with appropriate
/// responses.
async fn serve_search_provider(
    app_id: AppId,
    storage_tx: mpsc::Sender<(Span, oneshot::Sender<Result<StorageOpenedPathsList>>)>,
    mut rx: mpsc::Receiver<AppItemSearchRequest>,
) {
    let mut items = None;
    loop {
        match rx.next().await {
            None => {
                event!(Level::DEBUG, %app_id, "No more requests from search provider, stopping");
                break;
            }
            Some(request) => {
                let span = request.span().clone();
                items = handle_search_provider_request(
                    app_id.clone(),
                    storage_tx.clone(),
                    items,
                    request,
                )
                .instrument(span)
                .await;
            }
        }
    }
}

/// Starts the DBUS service loop.
///
/// Connect to the ession bus and register DBus objects for every provider
/// whose underlying VSCode variant is installed.
///
/// Then register the connection on the Glib main loop and handle incoming messages.
async fn start_dbus_service(log_control: LogControl) -> Result<Service> {
    let app_launch_service = AppLaunchService::new();
    // Create providers for all apps we find
    let user_config_dir = glib::user_config_dir();
    event!(Level::INFO, "Looking for installed apps");
    let mut providers = Vec::with_capacity(PROVIDERS.len());
    for provider in PROVIDERS {
        if let Some(gio_app) = gio::DesktopAppInfo::new(provider.desktop_id) {
            event!(Level::INFO, "Found app {}", provider.desktop_id);
            let (tx, rx) = mpsc::channel(8);
            let search_provider =
                AppItemSearchProvider::new(gio_app.into(), app_launch_service.client(), tx);
            let config_dir = user_config_dir.join(provider.config.dirname);
            let storage =
                GlobalStorage::open_from_config_directory(&config_dir).with_context(|| {
                    format!("Failed to open global storage in {}", config_dir.display())
                })?;
            let (storage_tx, storage_rx) = mpsc::channel(8);
            glib::MainContext::ref_thread_default().spawn(serve_search_provider(
                search_provider.app().id().clone(),
                storage_tx,
                rx,
            ));
            std::thread::spawn(move || {
                for (span, respond_to) in block_on_stream(storage_rx) {
                    span.in_scope(|| {
                        let _ = respond_to.send(storage.recently_opened_paths_list());
                    })
                }
            });
            providers.push((provider.objpath(), search_provider));
        } else {
            event!(
                Level::DEBUG,
                desktop_id = provider.desktop_id,
                "Skipping provider, app not found"
            );
        }
    }

    event!(
        Level::INFO,
        "Registering {} search provider(s) on {}",
        providers.len(),
        BUSNAME
    );
    let connection = providers
        .into_iter()
        .try_fold(
            zbus::ConnectionBuilder::session()?,
            |b, (path, provider)| {
                event!(
                Level::DEBUG,
                app_id=%provider.app().id(),
                "Registering search provider for app {} at {}",
                provider.app().id(),
                path
                );
                b.serve_at(path, provider)
            },
        )?
        .serve_at("/org/freedesktop/LogControl1", log_control)?
        .name(BUSNAME)?
        // We disable the internal executor because we'd like to run the connection
        // exclusively on the glib mainloop, and thus tick it manually (see below).
        .internal_executor(false)
        .build()
        .await
        .with_context(|| "Failed to connect to session bus")?;

    // Manually tick the connection on the glib mainloop to make all code in zbus run on the mainloop.
    glib::MainContext::ref_thread_default().spawn(tick(connection.clone()));

    event!(
        Level::INFO,
        "Acquired name {}, serving search providers",
        BUSNAME
    );
    Ok(Service {
        app_launch_service,
        connection,
    })
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

fn main() {
    let matches = app().get_matches();
    if matches.get_flag("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{label}")
        }
    } else {
        let log_control = setup_logging_for_service();

        event!(
            Level::INFO,
            "Started {} version: {}",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION")
        );

        match glib::MainContext::ref_thread_default().block_on(start_dbus_service(log_control)) {
            Ok(service) => {
                let _ = service.app_launch_service.start(
                    service.connection,
                    SystemdScopeSettings {
                        prefix: concat!("app-", env!("CARGO_BIN_NAME")).to_string(),
                        started_by: env!("CARGO_BIN_NAME").to_string(),
                        documentation: vec![env!("CARGO_PKG_HOMEPAGE").to_string()],
                    },
                );
                create_main_loop(&glib::MainContext::ref_thread_default()).run();
            }
            Err(error) => {
                event!(Level::ERROR, "Failed to start DBus server: {:#}", error);
                std::process::exit(1);
            }
        }
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
