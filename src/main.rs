// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::fmt::Debug;
use std::path::Path;
use std::rc::Rc;

use gio::{prelude::*, AppLaunchContext, DBusInterfaceInfo, DesktopAppInfo, IOErrorEnum};
use gio::{ApplicationFlags, DBusNodeInfo};
use glib::{UriFlags, Variant, VariantDict, VariantTy};
use rusqlite::{OpenFlags, OptionalExtension};
use serde::Deserialize;

static G_LOG_DOMAIN: &str = "VSCodeSearchProvider";

/// The literal XML definition of the interface.
static SEARCH_PROVIDER2_XML: &str = include_str!("../dbus-1/org.gnome.ShellSearchProvider2.xml");

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
pub struct StorageOpenedPathsList {
    entries: Option<Vec<StorageOpenedPathsListEntry>>,
}

struct QueryOpenedPathsLists {
    tx: async_channel::Sender<Result<Option<StorageOpenedPathsList>, glib::Error>>,
}

struct StorageClient {
    tx: async_channel::Sender<QueryOpenedPathsLists>,
}

fn query_recently_opened_path_lists(
    connection: &rusqlite::Connection,
) -> Result<Option<StorageOpenedPathsList>, glib::Error> {
    connection
        .query_row_and_then(
            "SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList';",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            glib::Error::new(
                IOErrorEnum::Failed,
                &format!(
                    "Failed to query recently opened path lists from VSCode global storage: {}",
                    error
                ),
            )
        })?
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                glib::Error::new(
                    IOErrorEnum::InvalidData,
                    &format!(
                        "Failed to deserialize recently opened path lists: {}",
                        error
                    ),
                )
            })
        })
        .transpose()
}

impl StorageClient {
    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self, glib::Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection =
            rusqlite::Connection::open_with_flags(db_path.as_ref(), flags).map_err(|error| {
                glib::Error::new(
                    IOErrorEnum::Failed,
                    &format!(
                        "Failed to open connection to {}: {error}",
                        db_path.as_ref().display()
                    ),
                )
            })?;
        let (tx, rx) = async_channel::bounded::<QueryOpenedPathsLists>(1);
        std::thread::spawn(move || {
            let context = glib::MainContext::new();
            let _guard = context.acquire().unwrap();
            context.block_on(async move {
                while let Ok(message) = rx.recv().await {
                    message
                        .tx
                        .send(query_recently_opened_path_lists(&connection))
                        .await
                        .unwrap();
                }
            });
        });
        Ok(Self { tx })
    }

    async fn query_recently_opened_path_lists(
        &self,
    ) -> Result<Option<StorageOpenedPathsList>, glib::Error> {
        let (tx, rx) = async_channel::bounded(1);
        self.tx.send(QueryOpenedPathsLists { tx }).await.unwrap();
        rx.recv().await.unwrap()
    }
}

#[derive(Debug, Variant)]
pub struct GetInitialResultSet {
    pub terms: Vec<String>,
}

#[derive(Debug, Variant)]
pub struct GetSubsearchResultSet {
    pub previous_results: Vec<String>,
    pub terms: Vec<String>,
}

#[derive(Debug, Variant)]
pub struct GetResultMetas {
    pub identifiers: Vec<String>,
}

#[derive(Debug, Variant)]
pub struct ActivateResult {
    pub identifier: String,
    pub terms: Vec<String>,
    pub timestamp: u32,
}

#[derive(Debug, Variant)]
pub struct LaunchSearch {
    pub terms: Vec<String>,
    pub timestamp: u32,
}

/// Method calls a search provider supports.
#[derive(Debug)]
pub enum SearchProvider2Method {
    GetInitialResultSet(GetInitialResultSet),
    GetSubsearchResultSet(GetSubsearchResultSet),
    GetResultMetas(GetResultMetas),
    ActivateResult(ActivateResult),
    LaunchSearch(LaunchSearch),
}

fn invalid_parameters() -> glib::Error {
    glib::Error::new(
        IOErrorEnum::InvalidArgument,
        "Invalid parameters for method",
    )
}

impl SearchProvider2Method {
    /// Parse a method call to a search provider.
    pub fn parse(
        method_name: &str,
        parameters: Variant,
    ) -> Result<SearchProvider2Method, glib::Error> {
        match method_name {
            "GetInitialResultSet" => parameters
                .get::<GetInitialResultSet>()
                .map(SearchProvider2Method::GetInitialResultSet)
                .ok_or_else(invalid_parameters),
            "GetSubsearchResultSet" => parameters
                .get::<GetSubsearchResultSet>()
                .map(SearchProvider2Method::GetSubsearchResultSet)
                .ok_or_else(invalid_parameters),
            "GetResultMetas" => parameters
                .get::<GetResultMetas>()
                .map(SearchProvider2Method::GetResultMetas)
                .ok_or_else(invalid_parameters),
            "ActivateResult" => parameters
                .get::<ActivateResult>()
                .map(SearchProvider2Method::ActivateResult)
                .ok_or_else(invalid_parameters),
            "LaunchSearch" => parameters
                .get::<LaunchSearch>()
                .map(SearchProvider2Method::LaunchSearch)
                .ok_or_else(invalid_parameters),
            _ => Err(glib::Error::new(
                IOErrorEnum::InvalidArgument,
                "Unexpected method",
            )),
        }
    }
}

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
fn find_matching_uris<I, S>(uris: I, terms: &[S]) -> Vec<String>
where
    S: AsRef<str> + Debug,
    I: IntoIterator<Item = String>,
{
    let mut scored = uris
        .into_iter()
        .filter_map(|uri| {
            let decoded_uri = glib::Uri::parse(&uri, UriFlags::NONE)
                .ok()
                .map(|s| s.to_str());
            let scored_uri = decoded_uri
                .as_ref()
                .map_or_else(|| uri.as_str(), |s| s.as_str());
            let score = score_uri(scored_uri, terms);
            glib::trace!("URI {scored_uri} scores {score} against {terms:?}");
            if score <= 0.0 {
                None
            } else {
                Some((score, uri))
            }
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, _)| -((score * 1000.0) as i64));
    scored.into_iter().map(|(_, uri)| uri).collect::<Vec<_>>()
}

pub fn name_from_uri(uri_or_path: &str) -> Option<&str> {
    uri_or_path.split("/").filter(|seg| !seg.is_empty()).last()
}

struct SearchProvider {
    app: DesktopAppInfo,
    storage: StorageClient,
}

impl SearchProvider {
    fn new(app: DesktopAppInfo, storage: StorageClient) -> Self {
        Self { app, storage }
    }

    /// Handle the given search provider method `call`.
    ///
    /// Perform any side effects triggered by the call and return the appropriate
    /// result.
    async fn handle_call(
        &self,
        call: SearchProvider2Method,
    ) -> Result<Option<Variant>, glib::Error> {
        // TODO: Move launched app to separate scope!
        match call {
            SearchProvider2Method::GetInitialResultSet(GetInitialResultSet { terms }) => {
                glib::debug!("Searching for terms {terms:?}");
                let uris = self
                    .storage
                    .query_recently_opened_path_lists()
                    .await?
                    .unwrap_or_default()
                    .entries
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|entry| match entry {
                        StorageOpenedPathsListEntry::Workspace { workspace } => {
                            Some(workspace.config_path)
                        }
                        StorageOpenedPathsListEntry::Folder { uri } => Some(uri),
                        StorageOpenedPathsListEntry::File { .. } => None,
                    });

                Ok(Some(find_matching_uris(uris, terms.as_slice()).into()))
            }
            SearchProvider2Method::GetSubsearchResultSet(GetSubsearchResultSet {
                previous_results,
                terms,
            }) => {
                glib::debug!(
                    "Searching for terms {terms:?} in {} previosu results",
                    previous_results.len()
                );
                Ok(Some(
                    find_matching_uris(previous_results, terms.as_slice()).into(),
                ))
            }
            SearchProvider2Method::GetResultMetas(GetResultMetas { identifiers }) => {
                glib::debug!("Get metadata for {identifiers:?}");
                let metas: Vec<VariantDict> = identifiers
                    .into_iter()
                    .map(|uri| {
                        let metas = VariantDict::new(None);
                        metas.insert("id", uri.as_str());
                        match glib::Uri::parse(&uri, UriFlags::NONE).ok() {
                            Some(parsed_uri) => {
                                metas.insert(
                                    "name",
                                    name_from_uri(parsed_uri.path().as_str())
                                        .unwrap_or(uri.as_str()),
                                );
                                match parsed_uri.scheme().as_str() {
                                    "file:" if parsed_uri.host().is_none() => {
                                        metas.insert("description", parsed_uri.path().as_str());
                                    }
                                    _ => {
                                        metas.insert("description", parsed_uri.to_str().as_str());
                                    }
                                };
                            }
                            None => {
                                metas.insert("name", name_from_uri(&uri).unwrap_or(uri.as_str()));
                                metas.insert("description", uri.as_str());
                            }
                        }
                        if let Some(app_icon) = self.app.icon().and_then(|icon| icon.serialize()) {
                            metas.insert("icon", app_icon)
                        }
                        metas
                    })
                    .collect::<Vec<_>>();
                Ok(Some(metas.into()))
            }
            SearchProvider2Method::ActivateResult(ActivateResult { identifier, .. }) => {
                glib::info!(
                    "Launching application {} with URI {identifier}",
                    self.app.id().unwrap()
                );
                glib::spawn_future_local(
                    self.app
                        .launch_uris_future(&[identifier.as_str()], AppLaunchContext::NONE),
                );
                Ok(None)
            }
            SearchProvider2Method::LaunchSearch(_) => {
                glib::info!("Launching application {} directly", self.app.id().unwrap());
                glib::spawn_future_local(self.app.launch_uris_future(&[], AppLaunchContext::NONE));
                Ok(None)
            }
        }
    }

    /// Register this search provider under `object_path` on a DBus `connection`.
    ///
    /// Consume the search provider, as it gets moved into the callback closure for
    /// DBus invocations.
    fn register(
        self,
        connection: &gio::DBusConnection,
        object_path: &str,
        interface_info: &DBusInterfaceInfo,
    ) -> Result<gio::RegistrationId, glib::Error> {
        let search_provider = Rc::new(self);
        connection
            .register_object(object_path, interface_info)
            .method_call(move |_, _, _, _, method_name, parameters, invocation| {
                match SearchProvider2Method::parse(method_name, parameters) {
                    Ok(call) => {
                        let search_provider = search_provider.clone();
                        glib::spawn_future_local(async move {
                            match search_provider.handle_call(call).await {
                                Ok(Some(variant)) if variant.type_() != VariantTy::TUPLE => {
                                    invocation.return_value(Some(&(variant,).into()))
                                }
                                Ok(other) => invocation.return_value(other.as_ref()),
                                Err(error) => invocation.return_gerror(error),
                            }
                        });
                    }
                    Err(error) => invocation.return_gerror(error),
                }
            })
            .build()
    }
}

fn startup(app: &gio::Application) {
    let providers = [
        // The standard Arch Linux code package from community
        ("code-oss.desktop", "Code - OSS"),
        // The standard codium package on Linux from here: https://github.com/VSCodium/vscodium.
        // Should work for most Linux distributions packaged from here.
        ("codium.desktop", "VSCodium"),
        // The official install packages from https://code.visualstudio.com/download
        ("code.desktop", "Code"),
    ];

    let interface = DBusNodeInfo::for_xml(SEARCH_PROVIDER2_XML)
        .unwrap()
        .lookup_interface("org.gnome.Shell.SearchProvider2")
        .unwrap();
    let user_config_dir = glib::user_config_dir();

    let connection = app.dbus_connection().unwrap();
    for (desktop_id, config_dir_name) in providers {
        if let Some(vscode_app) = DesktopAppInfo::new(desktop_id) {
            let object_path = format!(
                "{}/{}",
                app.dbus_object_path().unwrap(),
                vscode_app.id().unwrap().replace(".desktop", "")
            );
            let db_path = user_config_dir
                .join(config_dir_name)
                .join("User")
                .join("globalStorage")
                .join("state.vscdb");
            glib::info!(
                "Found app {desktop_id} with db at {}, exposing at {object_path}",
                db_path.display()
            );
            match StorageClient::open(&db_path) {
                Ok(storage) => {
                    let provider = SearchProvider::new(vscode_app, storage);
                    if let Err(error) = provider.register(&connection, &object_path, &interface) {
                        glib::error!(
                            "Skipping {desktop_id}, failed to register on {}, {error}",
                            object_path,
                        );
                    }
                }
                Err(error) => {
                    glib::error!(
                        "Skipping {desktop_id}, failed to open DB connection for {}, {error}",
                        db_path.display()
                    );
                }
            }
        }
    }
}

pub fn main() -> glib::ExitCode {
    static LOGGER: glib::GlibLogger = glib::GlibLogger::new(
        glib::GlibLoggerFormat::Structured,
        glib::GlibLoggerDomain::CrateTarget,
    );
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    let app = gio::Application::builder()
        .application_id("de.swsnr.VSCodeSearchProvider")
        .flags(ApplicationFlags::IS_SERVICE)
        .build();

    let _guard = app.hold();
    app.connect_startup(startup);
    app.run()
}
