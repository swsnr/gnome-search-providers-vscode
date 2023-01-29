// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for VSCode editors.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_executor::block_on_stream;
use rusqlite::{OpenFlags, OptionalExtension};
use serde::Deserialize;
use tracing::{event, instrument, Level, Span};
use tracing_futures::Instrument;

use gnome_search_provider_common::app::*;
use gnome_search_provider_common::futures_channel::{mpsc, oneshot};
use gnome_search_provider_common::futures_util::{SinkExt, StreamExt};
use gnome_search_provider_common::gio;
use gnome_search_provider_common::gio::glib;
use gnome_search_provider_common::logging::*;
use gnome_search_provider_common::mainloop::*;
use gnome_search_provider_common::matching::*;
use gnome_search_provider_common::zbus;

#[derive(Debug, Deserialize)]
struct WorkspaceEntry {
    #[serde(rename = "configPath")]
    config_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StorageOpenedPathsListEntry {
    Workspace {
        workspace: WorkspaceEntry,
    },
    Folder {
        #[serde(rename = "folderUri")]
        uri: String,
    },
    File {
        #[serde(rename = "fileUri")]
        #[allow(dead_code)]
        uri: String,
    },
}

impl StorageOpenedPathsListEntry {
    /// Move this entry into a workspace URL.
    fn into_workspace_url(self) -> Option<String> {
        match self {
            Self::Workspace { workspace } => Some(workspace.config_path),
            Self::Folder { uri } => Some(uri),
            Self::File { .. } => None,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct StorageOpenedPathsList {
    entries: Option<Vec<StorageOpenedPathsListEntry>>,
}

impl StorageOpenedPathsList {
    fn into_workspace_urls(self) -> Vec<String> {
        event!(Level::TRACE, "Extracting workspace URLs from {:?}", self);
        self.entries
            .map(|e| {
                e.into_iter()
                    .filter_map(|entry| entry.into_workspace_url())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// VSCode global storage.
#[derive(Debug)]
struct GlobalStorage {
    connection: rusqlite::Connection,
}

impl GlobalStorage {
    /// Open a global storage database at the given path.
    fn open_file<P: AsRef<Path>>(file: P) -> rusqlite::Result<Self> {
        event!(
            Level::DEBUG,
            "Opening VSCode global storage at {}",
            file.as_ref().display()
        );
        Ok(Self {
            connection: rusqlite::Connection::open_with_flags(
                file,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?,
        })
    }

    /// Return the path of the global storage database in a VSCode configuration directory.
    fn database_path_in_config_dir<P: AsRef<Path>>(directory: P) -> PathBuf {
        directory
            .as_ref()
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }

    // Open the global storage database in the given configuration `directory`.
    fn open_from_config_directory<P: AsRef<Path>>(directory: P) -> rusqlite::Result<Self> {
        Self::open_file(Self::database_path_in_config_dir(directory))
    }

    /// Query recently opened path lists.
    #[instrument(skip(self))]
    fn recently_opened_paths_list(&self) -> Result<StorageOpenedPathsList> {
        event!(Level::DEBUG, "Querying global storage for workspace list");
        let result = self
            .connection
            .query_row_and_then(
                "SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList';",
                [],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| "Failed to query recently opened paths lists from global storage")?;
        if let Some(data) = result {
            serde_json::from_value(data)
                .with_context(|| "Failed to parse JSON data from recently opened paths lists")
        } else {
            Ok(Default::default())
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct ConfigLocation<'a> {
    dirname: &'a str,
}

/// A search provider to expose from this service.
struct ProviderDefinition<'a> {
    /// A human readable label for this provider.
    label: &'a str,
    /// The ID (that is, the filename) of the desktop file of the corresponding app.
    desktop_id: &'a str,
    /// The relative object path to expose this provider at.
    relative_obj_path: &'a str,
    /// The location of the configuration for this app.
    config: ConfigLocation<'a>,
}

impl ProviderDefinition<'_> {
    /// Gets the full object path for this provider.
    fn objpath(&self) -> String {
        format!("/de/swsnr/searchprovider/vscode/{}", self.relative_obj_path)
    }
}

/// Known search providers.
///
/// For each definition in this array a corresponding provider file must exist in
/// `providers/`; the file must refer to the same `desktop_id` and the same object path.
/// The object path must be unique for each desktop ID, to ensure that this service always
/// launches the right application associated with the search provider.
const PROVIDERS: &[ProviderDefinition] = &[
    // The standard Arch Linux code package from community
    ProviderDefinition {
        label: "Code OSS (Arch Linux)",
        desktop_id: "code-oss.desktop",
        relative_obj_path: "arch/codeoss",
        config: ConfigLocation {
            dirname: "Code - OSS",
        },
    },
    // The binary AUR package for visual studio code: https://aur.archlinux.org/packages/visual-studio-code-bin/
    ProviderDefinition {
        label: "Visual Studio Code (AUR package)",
        desktop_id: "visual-studio-code.desktop",
        relative_obj_path: "aur/visualstudiocode",
        config: ConfigLocation { dirname: "Code" },
    },
    // The standard codium package on Linux from here: https://github.com/VSCodium/vscodium.
    // Should work for most Linux distributions packaged from here.
    ProviderDefinition {
        label: "VSCodium",
        desktop_id: "codium.desktop",
        relative_obj_path: "codium",
        config: ConfigLocation {
            dirname: "VSCodium",
        },
    },
    // The official install packages from https://code.visualstudio.com/download.
    ProviderDefinition {
        label: "Visual Studio Code (Official package)",
        desktop_id: "code.desktop",
        relative_obj_path: "official/code",
        config: ConfigLocation { dirname: "Code" },
    },
];

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
    use super::{app, glib, GlobalStorage, PROVIDERS};

    #[test]
    fn verify_app() {
        app().debug_assert();
    }

    #[test]
    #[ignore]
    fn load_global_storage() {
        // FIXME: find a way to get this test working on Github CI
        let user_config_dir = glib::user_config_dir();
        let global_storage_db = PROVIDERS
            .iter()
            .find_map(|provider| {
                let dir = user_config_dir.join(provider.config.dirname);
                let storage_db = GlobalStorage::database_path_in_config_dir(dir);
                match std::fs::metadata(&storage_db) {
                    Ok(metadata) if metadata.is_file() => Some(storage_db),
                    _ => None,
                }
            })
            .expect("At least one provider required for this test");

        let storage = GlobalStorage::open_file(global_storage_db).unwrap();
        let list = storage.recently_opened_paths_list().unwrap();
        let entries = list.entries.unwrap_or_default();
        assert!(!entries.is_empty(), "Entries: {entries:?}");
    }

    mod providers {
        use crate::{BUSNAME, PROVIDERS};
        use anyhow::{anyhow, Context, Result};
        use std::collections::HashSet;
        use std::path::Path;

        struct ProviderFile {
            desktop_id: String,
            object_path: String,
            bus_name: String,
            version: String,
        }

        fn load_all_provider_files() -> Result<Vec<ProviderFile>> {
            let mut providers = Vec::new();
            let ini_files = globwalk::GlobWalkerBuilder::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("providers"),
                "*.ini",
            )
            .build()
            .unwrap();
            for entry in ini_files {
                let filepath = entry.unwrap().into_path();
                let mut ini = configparser::ini::Ini::new();
                ini.load(&filepath).map_err(|s| {
                    anyhow!("Failed to parse ini file at {}: {}", filepath.display(), s)
                })?;
                let provider = ProviderFile {
                    desktop_id: ini
                        .get("Shell Search Provider", "DesktopId")
                        .with_context(|| format!("DesktopId missing in {}", &filepath.display()))?,
                    object_path: ini
                        .get("Shell Search Provider", "ObjectPath")
                        .with_context(|| {
                            format!("ObjectPath missing in {}", &filepath.display())
                        })?,
                    bus_name: ini
                        .get("Shell Search Provider", "BusName")
                        .with_context(|| format!("BusName missing in {}", &filepath.display()))?,
                    version: ini
                        .get("Shell Search Provider", "Version")
                        .with_context(|| format!("Version missing in {}", &filepath.display()))?,
                };
                providers.push(provider);
            }

            Ok(providers)
        }

        #[test]
        fn all_providers_have_a_correct_ini_file() {
            let provider_files = load_all_provider_files().unwrap();
            for provider in PROVIDERS {
                let provider_file = provider_files
                    .iter()
                    .find(|p| p.desktop_id == provider.desktop_id);
                assert!(
                    provider_file.is_some(),
                    "Provider INI missing for provider {} with desktop ID {}",
                    provider.label,
                    provider.desktop_id
                );

                assert_eq!(provider_file.unwrap().object_path, provider.objpath());
                assert_eq!(provider_file.unwrap().bus_name, BUSNAME);
                assert_eq!(provider_file.unwrap().version, "2");
            }
        }

        #[test]
        fn no_extra_ini_files_without_providers() {
            let provider_files = load_all_provider_files().unwrap();
            assert_eq!(PROVIDERS.len(), provider_files.len());
        }

        #[test]
        fn desktop_ids_are_unique() {
            let mut ids = HashSet::new();
            for provider in PROVIDERS {
                ids.insert(provider.desktop_id);
            }
            assert_eq!(PROVIDERS.len(), ids.len());
        }

        #[test]
        fn dbus_paths_are_unique() {
            let mut paths = HashSet::new();
            for provider in PROVIDERS {
                paths.insert(provider.objpath());
            }
            assert_eq!(PROVIDERS.len(), paths.len());
        }
    }
}
