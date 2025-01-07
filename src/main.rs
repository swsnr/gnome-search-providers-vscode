// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::fmt::Debug;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gio::{
    prelude::*, AppLaunchContext, Application, DBusCallFlags, DBusInterfaceInfo, DBusProxyFlags,
    DesktopAppInfo, IOErrorEnum,
};
use gio::{ApplicationFlags, DBusNodeInfo};
use glib::{UriFlags, Variant, VariantDict};
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

#[derive(Debug, Variant)]
pub struct GetInitialResultSet(Vec<String>);

#[derive(Debug, Variant)]
pub struct GetSubsearchResultSet(Vec<String>, Vec<String>);

#[derive(Debug, Variant)]
pub struct GetResultMetas(Vec<String>);

#[derive(Debug, Variant)]
pub struct ActivateResult(String, Vec<String>, u32);

#[derive(Debug, Variant)]
pub struct LaunchSearch(Vec<String>, u32);

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

impl DBusMethodCall for SearchProvider2Method {
    fn parse_call(
        _obj_path: &str,
        _interface: Option<&str>,
        method: &str,
        params: glib::Variant,
    ) -> Result<Self, glib::Error> {
        match method {
            "GetInitialResultSet" => params
                .get::<GetInitialResultSet>()
                .map(SearchProvider2Method::GetInitialResultSet)
                .ok_or_else(invalid_parameters),
            "GetSubsearchResultSet" => params
                .get::<GetSubsearchResultSet>()
                .map(SearchProvider2Method::GetSubsearchResultSet)
                .ok_or_else(invalid_parameters),
            "GetResultMetas" => params
                .get::<GetResultMetas>()
                .map(SearchProvider2Method::GetResultMetas)
                .ok_or_else(invalid_parameters),
            "ActivateResult" => params
                .get::<ActivateResult>()
                .map(SearchProvider2Method::ActivateResult)
                .ok_or_else(invalid_parameters),
            "LaunchSearch" => params
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
fn find_matching_uris<I, U, S>(uris: I, terms: &[S]) -> Vec<U>
where
    S: AsRef<str> + Debug,
    U: AsRef<str>,
    I: IntoIterator<Item = U>,
{
    let mut scored = uris
        .into_iter()
        .filter_map(|uri| {
            let decoded_uri = glib::Uri::parse(uri.as_ref(), UriFlags::NONE)
                .ok()
                .map(|s| s.to_str());
            let scored_uri = decoded_uri
                .as_ref()
                .map_or_else(|| uri.as_ref(), |s| s.as_str());
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

/// Escape a systemd unit name.
///
/// See section "STRING ESCAPING FOR INCLUSION IN UNIT NAMES" in `systemd.unit(5)`
/// for details about the algorithm.
fn escape_name_for_systemd(name: &str) -> String {
    if name.is_empty() {
        "".to_string()
    } else {
        name.bytes()
            .enumerate()
            .map(|(n, b)| {
                let c = char::from(b);
                match c {
                    '/' => '-'.to_string(),
                    ':' | '_' | '0'..='9' | 'a'..='z' | 'A'..='Z' => c.to_string(),
                    '.' if n > 0 => c.to_string(),
                    _ => format!(r#"\x{b:02x}"#),
                }
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Variant)]
struct StartTransientUnitParameters {
    name: String,
    mode: String,
    properties: Vec<(String, Variant)>,
    aux: Vec<(String, Vec<(String, Variant)>)>,
}

/// Move `pid` into the given `scope`, as a new transient unit.
///
/// This isolates the process from the current one.
///
/// Return the object path of the new transient unit, on the systemd manager.
async fn move_to_scope(pid: i32, scope: String) -> Result<String, glib::Error> {
    let flags = DBusProxyFlags::DO_NOT_AUTO_START_AT_CONSTRUCTION
        | DBusProxyFlags::DO_NOT_CONNECT_SIGNALS
        | DBusProxyFlags::DO_NOT_LOAD_PROPERTIES;
    let systemd1 = gio::DBusProxy::for_bus_future(
        gio::BusType::Session,
        flags,
        None,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;

    // Properties of the new unit.  Note that we have to convert the property
    // value to a variant, and then box this variant in another variant, so that
    // the properties array has type a(sv) as per the manager1 interface.
    let properties = vec![
        // I haven't found any documentation for the type of the PIDs property directly, but elsewhere
        // in its DBus interface systemd always used u32 for PIDs.
        (
            "PIDs".to_string(),
            glib::Variant::from_variant(&vec![u32::try_from(pid).unwrap()].to_variant()),
        ),
        // libgnome passes this property too, see
        // https://gitlab.gnome.org/GNOME/gnome-desktop/-/blob/106a729c3f98b8ee56823a0a49fa8504f78dd355/libgnome-desktop/gnome-systemd.c#L100
        //
        // I'm not entirely sure how it's relevant but it seems a good idea to do what Gnome does.
        (
            "CollectMode".to_string(),
            glib::Variant::from_variant(&"inactive-or-failed".to_variant()),
        ),
    ];
    // Timeout for this DBus tool, chosen at a wild guess, and absolutely not
    // backed by any kind of data or experience :)
    let timeout = Duration::from_secs(1);
    let parameters = StartTransientUnitParameters {
        name: scope,
        mode: "fail".to_string(),
        properties,
        aux: Vec::new(),
    };
    glib::debug!("Calling StartTransientUnit with {parameters:?}");
    let reply = systemd1
        .call_future(
            "StartTransientUnit",
            Some(&parameters.into()),
            DBusCallFlags::NONE,
            timeout.as_millis() as i32,
        )
        .await?;
    Ok(reply.get::<(String,)>().unwrap().0)
}

struct SearchProvider {
    search_provider_app: Application,
    code_app: DesktopAppInfo,
    workspaces: Vec<String>,
    launch_context: AppLaunchContext,
}

impl SearchProvider {
    fn new(
        search_provider_app: Application,
        code_app: DesktopAppInfo,
        workspaces: Vec<String>,
    ) -> Self {
        let launch_context = AppLaunchContext::new();
        launch_context.connect_launched(glib::clone!(
            #[strong]
            search_provider_app,
            move |_, app, platform_data| {
                // Hold on to the search provider app while we're moving the new
                // process to its own scope.
                let guard = search_provider_app.hold();
                glib::info!(
                    "Launched app {} with platform data {platform_data:?}",
                    app.id().unwrap()
                );
                // The type of the pid property doesn't seem to be documented anywhere, but variant type
                // errors indicate that the type is "i", i.e.gint32.
                //
                // See https://docs.gtk.org/glib/gvariant-format-strings.html#numeric-types
                let pid = platform_data.get::<VariantDict>().and_then(|data| {
                    data.lookup::<i32>("pid")
                        .inspect_err(|error| {
                            glib::error!(
                                "platform_data.pid had type {} but expected type {}",
                                error.actual,
                                error.expected
                            )
                        })
                        .ok()
                        .flatten()
                });
                if let Some(pid) = pid {
                    let scope_name = format!(
                        "app-{}-{}-{}.scope",
                        env!("CARGO_BIN_NAME"),
                        escape_name_for_systemd(app.id().unwrap().trim_end_matches(".desktop")),
                        pid
                    );
                    glib::spawn_future_local(glib::clone!(async move {
                        match move_to_scope(pid, scope_name).await {
                            Ok(obj_path) => {
                                glib::info!("New process {pid} moved to scope at {obj_path:?}");
                            }
                            Err(error) => glib::error!(
                                "Failed to move process {pid} into a new scope: {error}"
                            ),
                        };
                        // Drop app only after the spawned process in its own scope
                        drop(guard);
                    }));
                }
            }
        ));
        Self {
            search_provider_app,
            code_app,
            workspaces,
            launch_context,
        }
    }

    /// Launch the given `uri`, if any, or launch the app directly.
    async fn launch_uri(&self, uri: Option<&str>) -> Result<(), glib::Error> {
        self.code_app
            .launch_uris_future(uri.as_slice(), Some(&self.launch_context))
            .await
    }

    /// Handle the given search provider method `call`.
    ///
    /// Perform any side effects triggered by the call and return the appropriate
    /// result.
    async fn handle_call(
        &self,
        call: SearchProvider2Method,
    ) -> Result<Option<Variant>, glib::Error> {
        // Hold on to the application while we're processing a DBus call.
        let _guard = self.search_provider_app.hold();
        match call {
            SearchProvider2Method::GetInitialResultSet(GetInitialResultSet(terms)) => {
                glib::debug!("Searching for terms {terms:?}");
                Ok(Some(
                    find_matching_uris(&self.workspaces, terms.as_slice()).into(),
                ))
            }
            SearchProvider2Method::GetSubsearchResultSet(GetSubsearchResultSet(
                previous_results,
                terms,
            )) => {
                glib::debug!(
                    "Searching for terms {terms:?} in {} previous results",
                    previous_results.len()
                );
                Ok(Some(
                    find_matching_uris(previous_results, terms.as_slice()).into(),
                ))
            }
            SearchProvider2Method::GetResultMetas(GetResultMetas(identifiers)) => {
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
                        if let Some(app_icon) =
                            self.code_app.icon().and_then(|icon| icon.serialize())
                        {
                            metas.insert("icon", app_icon)
                        }
                        metas
                    })
                    .collect::<Vec<_>>();
                Ok(Some(metas.into()))
            }
            SearchProvider2Method::ActivateResult(ActivateResult(identifier, _, _)) => {
                glib::info!(
                    "Launching application {} with URI {identifier}",
                    self.code_app.id().unwrap()
                );
                self.launch_uri(Some(identifier.as_ref())).await?;
                Ok(None)
            }
            SearchProvider2Method::LaunchSearch(_) => {
                glib::info!(
                    "Launching application {} directly",
                    self.code_app.id().unwrap()
                );
                self.launch_uri(None).await?;
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
            .typed_method_call::<SearchProvider2Method>()
            .invoke_and_return_future_local(move |_, _, call| {
                let search_provider = search_provider.clone();
                async move { search_provider.handle_call(call).await }
            })
            .build()
    }
}

/// Load workspaces from the given connection, and return all workspace URIs.
fn load_workspaces(connection: &rusqlite::Connection) -> Result<Vec<String>, glib::Error> {
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

fn open_connection<P: AsRef<Path>>(db_path: P) -> Result<rusqlite::Connection, glib::Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    rusqlite::Connection::open_with_flags(db_path.as_ref(), flags).map_err(|error| {
        glib::Error::new(
            IOErrorEnum::Failed,
            &format!(
                "Failed to open connection to {}: {error}",
                db_path.as_ref().display()
            ),
        )
    })
}

fn startup(app: &gio::Application) {
    // Hold on to the application during startup, to avoid early exit.
    let _guard = app.hold();

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
                vscode_app.id().unwrap().trim_end_matches(".desktop")
            );
            let db_path = user_config_dir
                .join(config_dir_name)
                .join("User")
                .join("globalStorage")
                .join("state.vscdb");
            glib::info!(
                "Found app {desktop_id}, loading workspaces from db at {}",
                db_path.display()
            );
            match open_connection(&db_path).and_then(|c| load_workspaces(&c)) {
                Ok(workspaces) => {
                    glib::info!("Found {} workspaces for {desktop_id}, exposing search provider at {object_path}", workspaces.len());
                    let provider = SearchProvider::new(app.clone(), vscode_app, workspaces);
                    if let Err(error) = provider.register(&connection, &object_path, &interface) {
                        glib::error!(
                            "Skipping {desktop_id}, failed to register on {}, {error}",
                            object_path,
                        );
                    }
                }
                Err(error) => {
                    glib::error!(
                        "Skipping {desktop_id}, failed to load workspaces from {}: {error}",
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
        // Exit one minute after release the app, i.e. in our case after finishing
        // the last DBus call.
        .inactivity_timeout(Duration::from_secs(60).as_millis().try_into().unwrap())
        .build();

    app.set_version(env!("CARGO_PKG_VERSION"));
    app.connect_startup(startup);
    app.run()
}
