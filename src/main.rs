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

use std::path::PathBuf;

use freedesktop_desktop_entry::DesktopEntry;
use futures_util::{select, FutureExt};
use logcontrol_tracing::{PrettyLogControl1LayerFactory, TracingLogControl1};
use logcontrol_zbus::ConnectionBuilderExt;
use searchprovider::SearchProvider;
use tokio::signal::{
    ctrl_c,
    unix::{signal, SignalKind},
};
use tracing::{debug, error, info, instrument, Level};
use tracing_subscriber::{layer::SubscriberExt, Registry};
use zbus::conn::Builder;

mod xdg {
    use std::path::PathBuf;

    use freedesktop_desktop_entry::DesktopEntry;

    fn user_home() -> PathBuf {
        std::env::var_os("HOME").unwrap().into()
    }

    /// Return `XDG_CONFIG_HOME`.
    pub fn config_home() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| user_home().join(".config"), Into::into)
    }

    /// Return `XDG_DATA_HOME`.
    pub fn data_home() -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| user_home().join(".local").join("share"), Into::into)
    }

    /// Return `XDG_DATA_DIRS`.
    pub fn data_dirs() -> Vec<PathBuf> {
        match std::env::var_os("XDG_DATA_DIRS") {
            Some(dirs) => std::env::split_paths(&dirs).map(Into::into).collect(),
            None => vec!["/usr/local/share/".into(), "/usr/share/".into()],
        }
    }

    pub fn find_desktop_entry(app_id: &str) -> Option<DesktopEntry> {
        let mut data_dirs = data_dirs();
        let mut dirs = Vec::with_capacity(data_dirs.len() + 1);
        dirs.push(data_home());
        dirs.append(&mut data_dirs);
        dirs.into_iter()
            .map(|d| {
                d.join("applications")
                    .join(app_id)
                    .with_extension("desktop")
            })
            .find_map(|file| DesktopEntry::from_path::<&str>(file, None).ok())
    }
}

mod workspaces {
    use std::io::{Error, ErrorKind, Result};
    use std::path::Path;

    use rusqlite::{OpenFlags, OptionalExtension};
    use serde::Deserialize;
    use tracing::{debug, error, instrument};

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

    #[derive(Debug, Deserialize, Default)]
    struct StorageOpenedPathsList {
        entries: Option<Vec<StorageOpenedPathsListEntry>>,
    }

    fn query_recently_opened_path_lists(
        connection: &rusqlite::Connection,
    ) -> Result<Option<StorageOpenedPathsList>> {
        connection
            .query_row_and_then(
                "SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList';",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| Error::new(ErrorKind::Other, error))?
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| Error::new(ErrorKind::InvalidData, error))
            })
            .transpose()
    }

    fn load_workspaces(connection: &rusqlite::Connection) -> Result<Vec<String>> {
        Ok(query_recently_opened_path_lists(connection)?
            .unwrap_or_default()
            .entries
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| match entry {
                StorageOpenedPathsListEntry::Workspace { workspace } => Some(workspace.config_path),
                StorageOpenedPathsListEntry::Folder { uri } => Some(uri),
                StorageOpenedPathsListEntry::File { .. } => None,
            })
            .collect())
    }

    fn open_connection<P: AsRef<Path>>(db_path: P) -> Result<rusqlite::Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        rusqlite::Connection::open_with_flags(db_path.as_ref(), flags).map_err(|error| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "Failed to open connection to {}: {error}",
                    db_path.as_ref().display()
                ),
            )
        })
    }

    #[instrument(fields(db_path = %db_path.as_ref().display()))]
    pub fn load_workspaces_from_path<P: AsRef<Path>>(db_path: P) -> Result<Vec<String>> {
        debug!("Loading workspaces from {}", db_path.as_ref().display());
        let connection = open_connection(db_path.as_ref())?;
        load_workspaces(&connection)
            .inspect(|workspaces| {
                debug!("Found {} workspaces", workspaces.len());
            })
            .inspect_err(|error| {
                error!(
                    "Failed to load workspaces from {}: {error}",
                    db_path.as_ref().display()
                );
            })
    }
}

mod search {
    use std::fmt::Debug;

    use tracing::{instrument, trace, warn};
    use url::Url;

    /// Calculate how well `uri` matches all of the given `terms`.
    ///
    /// The URI gets scored for each term according to how far to the right it appears in the URI,
    /// under the assumption that the right most part of an URI path is the most specific.
    ///
    /// All matches are done on the lowercase text, i.e. case-insensitive.
    ///
    /// Return a positive score if all of `terms` match `uri`.  The higher the score the
    /// better the match, in relation to other matching values.  In and by itself however
    /// the score has no intrinsic meaning.
    ///
    /// If one term out of `terms` does not match `uri` return a score of 0, regardless
    /// of how well other terms match.
    #[allow(
        clippy::cast_precision_loss,
        reason = "terms won't grow so large as to cause issues in f64 conversion"
    )]
    fn score_uri<S: AsRef<str>>(uri: &str, terms: &[S]) -> f64 {
        let uri = uri.to_lowercase();
        terms
            .iter()
            .try_fold(0.0, |score, term| {
                uri.rfind(&term.as_ref().to_lowercase())
                    // We add 1 to avoid returning zero if the term matches right at the beginning.
                    .map(|index| score + ((index + 1) as f64 / uri.len() as f64))
            })
            .unwrap_or(0.0)
    }

    /// Find all URIs from `uris` which match all of `terms`.
    ///
    /// Score every URI, and filter out all URIs with a score of 0 or less.
    pub fn find_matching_uris<I, U, S>(uris: I, terms: &[S]) -> Vec<U>
    where
        S: AsRef<str> + Debug,
        U: AsRef<str>,
        I: IntoIterator<Item = U>,
    {
        let mut scored = uris
            .into_iter()
            .filter_map(|uri| {
                let decoded_uri = Url::parse(uri.as_ref()).ok().map(|s| s.to_string());
                let scored_uri = decoded_uri
                    .as_ref()
                    .map_or_else(|| uri.as_ref(), |s| s.as_str());
                let score = score_uri(scored_uri, terms);
                trace!("URI {scored_uri} scores {score} against {terms:?}");
                if score <= 0.0 {
                    None
                } else {
                    Some((score, uri))
                }
            })
            .collect::<Vec<_>>();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::as_conversions,
            reason = "Truncation intended to calculate a coarse ordering score"
        )]
        scored.sort_by_key(|(score, _)| -((score * 1000.0) as i64));
        scored.into_iter().map(|(_, uri)| uri).collect::<Vec<_>>()
    }

    fn name_from_uri(uri_or_path: &str) -> Option<&str> {
        uri_or_path.split('/').filter(|seg| !seg.is_empty()).last()
    }

    /// Get the name and description for the given workspace URI or path.
    #[instrument]
    pub fn name_and_description_of_uri(uri_or_path: &str) -> (String, String) {
        match Url::parse(uri_or_path) {
            Ok(parsed_uri) => {
                let name = name_from_uri(parsed_uri.path())
                    .unwrap_or(uri_or_path)
                    .to_owned();
                let description = match parsed_uri.scheme() {
                    "file:" if parsed_uri.host().is_none() => parsed_uri.path().into(),
                    _ => parsed_uri.to_string(),
                };
                (name, description)
            }
            Err(error) => {
                warn!("Failed to parse {uri_or_path} as URI: {error}");
                let name = name_from_uri(uri_or_path)
                    .unwrap_or(uri_or_path)
                    .to_string();
                let description = uri_or_path.to_string();
                (name, description)
            }
        }
    }
}

mod searchprovider {
    use std::io::{Error, ErrorKind};

    use freedesktop_desktop_entry::DesktopEntry;
    use futures_util::future::join_all;
    use serde::Serialize;
    use tokio::{process::Command, sync::OnceCell};
    use tracing::{debug, info, instrument, Span};
    use url::Url;
    use zbus::{
        interface,
        zvariant::{Array, OwnedValue, SerializeDict, Str, Type},
    };

    use super::{search, workspaces, CodeVariant};

    #[derive(Debug, Type, Serialize)]
    #[zvariant(signature = "(sv)")]
    struct SerializedIcon(&'static str, OwnedValue);

    impl SerializedIcon {
        fn from_desktop_entry(entry: &DesktopEntry) -> Option<Self> {
            let icon = entry.icon()?;
            let serialized = match Url::from_file_path(icon) {
                Ok(url) => Self("file", OwnedValue::from(Str::from(url.as_ref()))),
                Err(()) => Self(
                    "themed",
                    Array::from(vec![Str::from(icon), Str::from(format!("{icon}-symbolic"))])
                        .try_into()
                        .unwrap(),
                ),
            };
            Some(serialized)
        }
    }

    #[derive(Debug, Default, SerializeDict, Type)]
    #[zvariant(signature = "a{sv}")]
    struct ResultMeta {
        id: String,
        name: String,
        description: String,
        icon: Option<SerializedIcon>,
    }

    pub struct SearchProvider {
        code: CodeVariant,
        desktop_entry: OnceCell<Option<DesktopEntry<'static>>>,
    }

    impl SearchProvider {
        pub fn new(code: CodeVariant) -> Self {
            Self {
                code,
                desktop_entry: OnceCell::new(),
            }
        }

        /// Launch the given `uri`, if any, or launch the app directly.
        ///
        /// Launch the uri with this code via `gio launch` wrapped in `systemd-run`,
        /// to make damn sure that Visual Studio Code gets its own scope.
        ///
        /// We cannot launch the desktop app file directly, e.g. with `launch_uris`,
        /// and the move the new process to a separate scope using sytemd's D-Bus
        /// API because vscode aggressively forks into background so fast, that we
        /// will have lost track of its forked children before we get a chance to
        /// move the whole process tree to a new scope.  This effectively means that
        /// the actual Visual Studio Code process which shows the window then
        /// remains a child of our own service scope, and lives and dies with the
        /// process of this search provider service.  And since we auto-quit our
        /// service after a few idle minutes we'd take down open Visual Studio Code
        /// windows with us.
        ///
        /// Since we can't get this down race-free via Gio/GLib itself, spawn a new
        /// scope first with systemd-run and then spawn the app in with gio launch.]
        #[instrument(skip(self), fields(app_id = self.code.app_id))]
        async fn launch_uri(&self, uri: Option<&str>) -> Result<(), std::io::Error> {
            let desktop_entry = self.desktop_entry().await.ok_or(Error::new(
                ErrorKind::NotFound,
                format!("Application {} not found", self.code.app_id),
            ))?;
            // TODO: Random scope name using the app id
            Command::new("/usr/bin/systemd-run")
                // .arg("--unit")
                // .arg(format!(
                //     "app-gnome-{}-{}",
                //     self.code.app_id.replace('-', "_"),
                //     "398725203"
                // ))
                .args(["--user", "--scope", "--same-dir", "/usr/bin/gio", "launch"])
                .arg(desktop_entry.path.as_os_str())
                .args(uri.as_slice())
                .spawn()?;
            Ok(())
        }

        async fn desktop_entry(&self) -> Option<&DesktopEntry> {
            self.desktop_entry
                .get_or_init(|| async {
                    let code = self.code;
                    let span = Span::current();
                    let result = tokio::task::spawn_blocking(move || {
                        span.in_scope(|| code.find_desktop_entry())
                    })
                    .await;
                    match result {
                        Ok(result) => result,
                        // blocking tasks can't be cancelled anyway so we can safely convert into panic
                        Err(err) => std::panic::resume_unwind(err.into_panic()),
                    }
                })
                .await
                .as_ref()
        }

        async fn get_icon(&self) -> Option<SerializedIcon> {
            self.desktop_entry()
                .await
                .and_then(|entry| SerializedIcon::from_desktop_entry(entry))
        }

        #[instrument(skip(self))]
        async fn get_result_meta(&self, uri: &str) -> ResultMeta {
            let (name, description) = search::name_and_description_of_uri(uri);
            ResultMeta {
                id: uri.to_string(),
                name,
                description,
                icon: self.get_icon().await,
            }
        }

        #[instrument(skip(self))]
        async fn load_workspaces(&self) -> std::io::Result<Vec<String>> {
            let db_path = self.code.database_path();
            let span = Span::current();
            let result = tokio::task::spawn_blocking(move || {
                span.in_scope(|| workspaces::load_workspaces_from_path(&db_path))
            })
            .await;
            match result {
                Ok(result) => result,
                // blocking tasks can't be cancelled anyway so we can safely convert into panic
                Err(err) => std::panic::resume_unwind(err.into_panic()),
            }
        }
    }

    #[interface(name = "org.gnome.Shell.SearchProvider2", introspection_docs = false)]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    impl SearchProvider {
        #[instrument(skip(self))]
        async fn get_initial_result_set(&self, terms: Vec<&str>) -> zbus::fdo::Result<Vec<String>> {
            debug!("Searching for terms {terms:?}");
            let workspaces = self
                .load_workspaces()
                .await
                .map_err(|error: std::io::Error| zbus::fdo::Error::IOError(error.to_string()))?;
            Ok(search::find_matching_uris(workspaces, &terms))
        }

        #[instrument(skip(self))]
        fn get_subsearch_result_set(
            &self,
            previous_results: Vec<String>,
            terms: Vec<&str>,
        ) -> Vec<String> {
            debug!(
                "Searching for terms {terms:?} in {} previous results",
                previous_results.len()
            );
            search::find_matching_uris(previous_results, &terms)
        }

        #[instrument(skip(self))]
        async fn get_result_metas(&self, identifiers: Vec<String>) -> Vec<ResultMeta> {
            join_all(identifiers.iter().map(|uri| self.get_result_meta(uri))).await
        }

        #[instrument(skip(self))]
        async fn activate_result(
            &self,
            identifier: &str,
            _terms: Vec<&str>,
            _timestamp: u32,
        ) -> zbus::fdo::Result<()> {
            info!(
                "Launching application {} with URI {identifier}",
                self.code.app_id
            );
            self.launch_uri(Some(identifier))
                .await
                .map_err(|error: std::io::Error| zbus::fdo::Error::IOError(error.to_string()))?;
            Ok(())
        }

        #[instrument(skip(self))]
        async fn launch_search(&self, _terms: Vec<&str>, _timestamp: u32) -> zbus::fdo::Result<()> {
            info!("Launching application {} directly", self.code.app_id);
            self.launch_uri(None)
                .await
                .map_err(|error: std::io::Error| zbus::fdo::Error::IOError(error.to_string()))?;
            Ok(())
        }
    }
}

#[derive(Copy, Clone)]
struct CodeVariant {
    app_id: &'static str,
    config_directory_name: &'static str,
}

impl CodeVariant {
    fn database_path(&self) -> PathBuf {
        // Linux always has a config directory so we can safely unwrap here.
        xdg::config_home()
            .join(self.config_directory_name)
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }

    #[instrument(skip(self), fields(app_id = self.app_id))]
    fn find_desktop_entry(&self) -> Option<DesktopEntry<'static>> {
        xdg::find_desktop_entry(self.app_id).inspect(|desktop_entry| {
            debug!(
                "Found desktop entry {} for {}",
                desktop_entry.path.display(),
                self.app_id,
            );
        })
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
    } else {
        Level::INFO
    };
    let (control, control_layer) =
        TracingLogControl1::new_auto(PrettyLogControl1LayerFactory, default_level)?;
    let subscriber = Registry::default().with(env_filter).with(control_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let connection = Builder::session()?
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
        .build()
        .await?;
    info!("Connected to bus, serving search provider");

    let mut sigterm = signal(SignalKind::terminate())?;
    select! {
        result = ctrl_c().fuse() => {
            if let Err(error) = result {
                error!("Ctrl-C failed? {error}");
            } else {
                info!("Interrupted");
            }
        }
        _ = sigterm.recv().fuse() => {
            info!("Terminated");
        }
    }

    info!("Closing DBus connection");
    connection.close().await?;

    info!("Exiting");
    Ok(())
}
