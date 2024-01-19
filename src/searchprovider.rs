// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The search provider service for recent VSCode workspaces.

use crate::storage::GlobalStorage;
use crate::systemd;
use crate::systemd::Systemd1ManagerProxy;
use anyhow::{anyhow, Context, Result};
use gio::{prelude::*, Cancellable};
use glib::once_cell::unsync::Lazy;
use glib::{Variant, VariantDict};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::Mutex;
use tracing::{event, instrument, span, Level, Span};
use tracing_futures::Instrument;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{dbus_interface, zvariant};

/// The desktop ID of an app.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AppId(String);

impl Display for AppId {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<&AppId> for gio::DesktopAppInfo {
    type Error = glib::Error;

    fn try_from(value: &AppId) -> Result<Self, Self::Error> {
        gio::DesktopAppInfo::new(&value.0).ok_or_else(|| {
            glib::Error::new(
                glib::FileError::Noent,
                &format!("App {} not found", value.0),
            )
        })
    }
}

impl From<String> for AppId {
    fn from(v: String) -> Self {
        Self(v)
    }
}

impl From<&str> for AppId {
    fn from(v: &str) -> Self {
        v.to_string().into()
    }
}

impl From<&gio::DesktopAppInfo> for AppId {
    fn from(app: &gio::DesktopAppInfo) -> Self {
        AppId(app.id().unwrap().to_string())
    }
}

/// An app that can be launched.
#[derive(Debug)]
pub struct App {
    /// The ID of this app
    id: AppId,
    /// The icon to use for this app
    icon: String,
}

impl App {
    /// The ID of this app.
    pub fn id(&self) -> &AppId {
        &self.id
    }

    /// The icon of this app.
    pub fn icon(&self) -> &str {
        &self.icon
    }
}

impl From<gio::DesktopAppInfo> for App {
    fn from(app: gio::DesktopAppInfo) -> Self {
        Self {
            id: (&app).into(),
            icon: IconExt::to_string(&app.icon().unwrap())
                .unwrap()
                .to_string(),
        }
    }
}

/// A recent project from a Jetbrains IDE.
///
/// Note that rider calls these solutions per dotnet lingo.
#[derive(Debug, PartialEq, Eq)]
pub struct VSCodeRecentWorkspace {
    /// The human readable name of a workspace, as extracted from the `url`.
    name: String,

    /// The workspace URL, as read from the global storage of VSCode.
    url: String,
}

impl VSCodeRecentWorkspace {
    pub fn from_url(url: String) -> Result<VSCodeRecentWorkspace> {
        if let Some(name) = url.split('/').last() {
            let item = VSCodeRecentWorkspace {
                name: name.to_string(),
                url,
            };
            event!(Level::TRACE, "Found recent workspace item {:?}", item);
            Ok(item)
        } else {
            Err(anyhow!("Failed to extract workspace name from URL {}", url))
        }
    }
}

fn get_pid(platform_data: &Variant) -> Option<i32> {
    match platform_data.get::<VariantDict>() {
        None => {
            event!(
                Level::ERROR,
                "platform_data not a dictionary, but {:?}",
                platform_data
            );
            None
        }
        // The type of the pid property doesn't seem to be documented anywhere, but variant type
        // errors indicate that the type is "i", i.e.gint32.
        //
        // See https://docs.gtk.org/glib/gvariant-format-strings.html#numeric-types
        Some(data) => match data.lookup::<i32>("pid") {
            Err(type_error) => {
                event!(
                    Level::ERROR,
                    "platform_data.pid had type {:?}, but expected {:?}",
                    type_error.actual,
                    type_error.expected
                );
                None
            }
            Ok(None) => {
                event!(
                    Level::WARN,
                    "pid missing in platform_data {:?}",
                    platform_data
                );
                None
            }
            Ok(Some(pid)) => Some(pid),
        },
    }
}

#[instrument(skip(connection))]
async fn move_to_scope(
    connection: &zbus::Connection,
    app_name: &str,
    pid: u32,
) -> Result<(String, OwnedObjectPath), zbus::Error> {
    let manager = Systemd1ManagerProxy::new(connection).await?;
    // See https://gitlab.gnome.org/jf/start-transient-unit/-/blob/117c6f32c8dc0d1f28686408f698632aa71880bc/rust/src/main.rs#L94
    // for inspiration.
    // See https://www.freedesktop.org/wiki/Software/systemd/ControlGroupInterface/ for background.
    let props = &[
        // I haven't found any documentation for the type of the PIDs property directly, but elsewhere
        // in its DBus interface system always used u32 for PIDs.
        ("PIDs", Value::Array(vec![pid].into())),
        // libgnome passes this property too, see
        // https://gitlab.gnome.org/GNOME/gnome-desktop/-/blob/106a729c3f98b8ee56823a0a49fa8504f78dd355/libgnome-desktop/gnome-systemd.c#L100
        //
        // I'm not entirely sure how it's relevant but it seems a good idea to do what Gnome does.
        ("CollectMode", Value::Str("inactive-or-failed".into())),
    ];
    let name = format!(
        "app-{}-{}-{}.scope",
        env!("CARGO_BIN_NAME"),
        systemd::escape_name(app_name.trim_end_matches(".desktop")),
        pid
    );
    event!(
        Level::DEBUG,
        "Creating new scope {name} for PID {pid} of {app_name} with {props:?}"
    );
    let scope_object_path = manager
        .start_transient_unit(&name, "fail", props, &[])
        .await?;
    Ok((name, scope_object_path))
}

/// Launch the given app, optionally passing a given URI.
///
/// Move the launched app to a dedicated systemd scope for resource control, and return the result
/// of launching the app.
#[instrument(skip(connection))]
async fn launch_app_in_new_scope(
    connection: zbus::Connection,
    app_id: AppId,
    uri: Option<String>,
) -> zbus::fdo::Result<()> {
    let context = Lazy::new(|| {
        let context = gio::AppLaunchContext::new();
        context.connect_launched(move |_, app, platform_data| {
            let app_id = app.id().unwrap().to_string();
            let _guard = span!(Level::INFO, "launched", %app_id, %platform_data).entered();
            event!(
                Level::TRACE,
                "App {} launched with platform_data: {:?}",
                app_id,
                platform_data
            );
            if let Some(pid) = get_pid(platform_data) {
                event!(Level::INFO, "App {} launched with PID {pid}", app.id().unwrap());
                let app_name = app.id().unwrap().to_string();
                let connection_inner = connection.clone();
                glib::MainContext::ref_thread_default().spawn(
                    async move {
                        match move_to_scope(&connection_inner, &app_name, pid as u32).await {
                            Err(err) => {
                                event!(Level::ERROR, "Failed to move running process {pid} of app {app_name} into new systemd scope: {err}");
                            },
                            Ok((name, path)) => {
                                event!(Level::INFO, "Moved running process {pid} of app {app_name} into new systemd scope {name} at {}", path.into_inner());
                            },
                        }
                    }.in_current_span(),
                );
            }
        });
        context
    });

    let app = gio::DesktopAppInfo::try_from(&app_id).map_err(|error| {
        event!(
            Level::ERROR,
            %error,
            "Failed to find app {app_id}: {error:#}"
        );
        zbus::fdo::Error::Failed(format!("Failed to find app {app_id}: {error}"))
    })?;
    match uri {
        None => app.launch_uris_future(&[], Some(&*context)),
        Some(ref uri) => app.launch_uris_future(&[uri], Some(&*context)),
    }
    .await
    .map_err(|error| {
        event!(
            Level::ERROR,
            %error,
            "Failed to launch app {app_id} with {uri:?}: {error:#}",
        );
        zbus::fdo::Error::Failed(format!(
            "Failed to launch app {app_id} with {uri:?}: {error}"
        ))
    })
}

/// Check whether the given workspace exists.
///
/// For `file://` workspaces we check whether the workspace exists.  For other
/// schemes we simply assume that the workspace exists, because we have no
/// deeper understanding of VSCode URLs.
fn check_workspace_exists(url: &str) -> bool {
    let file = gio::File::for_uri(url);
    match file.uri_scheme().as_ref().map(|s| s.as_str()) {
        Some("file") => file.query_exists(Cancellable::NONE),
        _ => true,
    }
}

/// Read recent workspaces for the given app from the given storage.
fn read_recent_workspaces_from_storage(
    app_id: &AppId,
    storage: &GlobalStorage,
) -> Result<IndexMap<String, VSCodeRecentWorkspace>> {
    let workspace_urls = storage
        .recently_opened_paths_list()
        .with_context(|| {
            format!(
                "Failed to request recently opened paths from global storage of app {}",
                app_id
            )
        })?
        .into_workspace_urls();

    let workspaces = workspace_urls
        .into_iter()
        .filter(|url| check_workspace_exists(url))
        .filter_map(|url| match VSCodeRecentWorkspace::from_url(url) {
            Ok(item) => {
                event!(Level::TRACE, "Found recent workspace at {}", &item.url);
                let id = format!("vscode-search-provider-{}-{}", app_id, &item.url);
                Some((id, item))
            }
            Err(error) => {
                event!(Level::WARN, "Skipping workspace: {}", error);
                None
            }
        })
        .collect::<IndexMap<_, _>>();

    event!(Level::INFO, %app_id, "Found {} recent workspace(s) for app {}", workspaces.len(), app_id);
    Ok(workspaces)
}

/// Calculate how well `item` matches all of the given `terms`.
///
/// If all terms match the name of the `item`, the item receives a base score of 10.
/// If all terms match the URI of the `item`, the items gets scored for each term according to
/// how far right the term appears in the URI, under the assumption that the right most part
/// of an URI path is the most specific.
///
/// All matches are done on the lowercase text, i.e. case-insensitive.
fn item_score(item: &VSCodeRecentWorkspace, terms: &[&str]) -> f64 {
    let name = item.name.to_lowercase();
    let directory = item.url.to_lowercase();
    terms
        .iter()
        .try_fold(0.0, |score, term| {
            directory
                .rfind(&term.to_lowercase())
                // We add 1 to avoid returning zero if the term matches right at the beginning.
                .map(|index| score + ((index + 1) as f64 / item.url.len() as f64))
        })
        .unwrap_or(0.0)
        + if terms.iter().all(|term| name.contains(&term.to_lowercase())) {
            10.0
        } else {
            0.0
        }
}

#[derive(Debug)]
pub struct VSCodeWorkspaceSearchProvider {
    app: App,
    recent_workspaces: IndexMap<String, VSCodeRecentWorkspace>,
    /// The storage to load workspaces from.
    ///
    /// Placed behind a mutex to make it Sync as required for the DBus interface.
    /// However, we never actually acquire the lock, as we're only accessing this
    /// from a &mut self context.
    storage: Mutex<GlobalStorage>,
}

impl VSCodeWorkspaceSearchProvider {
    /// Create a new search provider for a jetbrains product.
    ///
    /// `app` describes the underlying app to launch items with, and `storage` is the global database
    /// where VSCode stores its recent workspaces.
    pub fn new(app: App, storage: GlobalStorage) -> Self {
        Self {
            app,
            storage: Mutex::new(storage),
            recent_workspaces: IndexMap::new(),
        }
    }

    /// Get the underlying app for this VSCode variant..
    pub fn app(&self) -> &App {
        &self.app
    }

    /// Reload all recent workspaces provided by this search provider.
    #[instrument(skip(self), fields(app_id = %self.app.id()))]
    pub fn reload_recent_workspaces(&mut self) -> Result<()> {
        // We never acquire the the storage lock in fact, so it can't be poisoned,
        // and we can conveniently ignore a poison error here.
        let storage = self.storage.get_mut().unwrap();
        self.recent_workspaces = read_recent_workspaces_from_storage(self.app.id(), storage)?;
        Ok(())
    }

    /// Find all IDs matching terms, ordered by best match.
    pub fn find_ids_by_terms(&self, terms: &[&str]) -> Vec<&str> {
        let mut scored_ids = self
            .recent_workspaces
            .iter()
            .filter_map(|(id, item)| {
                let score = item_score(item, terms);
                if 0.0 < score {
                    Some((id.as_ref(), score))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        scored_ids.sort_by_key(|(_, score)| -((score * 1000.0) as i64));
        scored_ids.into_iter().map(|(id, _)| id).collect()
    }

    #[instrument(skip(self, connection), fields(app_id = %self.app.id()))]
    async fn launch_app_on_default_main_context(
        &self,
        connection: zbus::Connection,
        uri: Option<String>,
    ) -> zbus::fdo::Result<()> {
        let app_id = self.app.id().clone();
        let span = Span::current();
        glib::MainContext::default()
            .spawn_from_within(move || {
                launch_app_in_new_scope(connection, app_id, uri.clone()).instrument(span)
            })
            .await
            .map_err(|error| {
                event!(
                    Level::ERROR,
                    %error,
                    "Join from main loop failed: {error:#}",
                );
                zbus::fdo::Error::Failed(format!("Join from main loop failed: {error:#}",))
            })?
    }
}

/// The DBus interface of the search provider.
///
/// See <https://developer.gnome.org/SearchProvider/> for information.
#[dbus_interface(name = "org.gnome.Shell.SearchProvider2")]
impl VSCodeWorkspaceSearchProvider {
    /// Starts a search.
    ///
    /// This function is called when a new search is started. It gets an array of search terms as arguments,
    /// and should return an array of result IDs. gnome-shell will call GetResultMetas for (some) of these result
    /// IDs to get details about the result that can be be displayed in the result list.
    #[instrument(skip(self), fields(app_id = %self.app.id()))]
    fn get_initial_result_set(&mut self, terms: Vec<&str>) -> Vec<&str> {
        event!(Level::DEBUG, "Reloading recent workspaces");
        if let Err(error) = self.reload_recent_workspaces() {
            event!(
                Level::ERROR,
                "Failed to reload recent workspaces: {}",
                error
            );
        }
        event!(Level::DEBUG, "Searching for {:?}", terms);
        let ids = self.find_ids_by_terms(&terms);
        event!(Level::DEBUG, "Found ids {:?}", ids);
        ids
    }

    /// Refine an ongoing search.
    ///
    /// This function is called to refine the initial search results when the user types more characters in the search entry.
    /// It gets the previous search results and the current search terms as arguments, and should return an array of result IDs,
    /// just like GetInitialResultSet.
    #[instrument(skip(self), fields(app_id = %self.app.id()))]
    fn get_subsearch_result_set(&self, previous_results: Vec<&str>, terms: Vec<&str>) -> Vec<&str> {
        event!(
            Level::DEBUG,
            "Searching for {:?} in {:?}",
            terms,
            previous_results
        );
        // For simplicity just run the overall search again, and filter out everything not already matched.
        let ids = self
            .find_ids_by_terms(&terms)
            .into_iter()
            .filter(|id| previous_results.contains(id))
            .collect();
        event!(Level::DEBUG, "Found ids {:?}", ids);
        ids
    }

    /// Get metadata for results.
    ///
    /// This function is called to obtain detailed information for results.
    /// It gets an array of result IDs as arguments, and should return a matching array of dictionaries
    /// (ie one a{sv} for each passed-in result ID).
    ///
    /// The following pieces of information should be provided for each result:
    //
    //  - "id": the result ID
    //  - "name": the display name for the result
    //  - "icon": a serialized GIcon (see g_icon_serialize()), or alternatively,
    //  - "gicon": a textual representation of a GIcon (see g_icon_to_string()), or alternatively,
    //  - "icon-data": a tuple of type (iiibiiay) describing a pixbuf with width, height, rowstride, has-alpha, bits-per-sample, and image data
    //  - "description": an optional short description (1-2 lines)
    #[instrument(skip(self), fields(app_id = %self.app.id()))]
    fn get_result_metas(
        &self,
        results: Vec<String>,
    ) -> zbus::fdo::Result<Vec<HashMap<String, zvariant::Value<'_>>>> {
        event!(Level::DEBUG, "Getting meta info for {:?}", results);
        let mut metas = Vec::with_capacity(results.len());
        for item_id in results {
            if let Some(item) = self.recent_workspaces.get(&item_id) {
                event!(Level::DEBUG, %item_id, "Compiling meta info for {}", item_id);
                let mut meta: HashMap<String, zvariant::Value> = HashMap::new();
                meta.insert("id".to_string(), item_id.clone().into());
                meta.insert("name".to_string(), item.name.clone().into());
                event!(Level::DEBUG, %item_id, "Using icon {}", self.app.icon());
                meta.insert("gicon".to_string(), self.app.icon().to_string().into());
                meta.insert("description".to_string(), item.url.clone().into());
                metas.push(meta);
            }
        }
        event!(Level::DEBUG, "Return meta info {:?}", &metas);
        Ok(metas)
    }

    /// Activate an individual result.
    ///
    /// This function is called when the user clicks on an individual result to open it in the application.
    /// The arguments are the result ID, the current search terms and a timestamp.
    ///
    /// Launches the underlying app with the path to the selected item.
    #[instrument(skip(self, connection), fields(app_id = %self.app.id()))]
    async fn activate_result(
        &mut self,
        #[zbus(connection)] connection: &zbus::Connection,
        item_id: &str,
        terms: Vec<&str>,
        timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        event!(
            Level::DEBUG,
            item_id,
            "Activating result {} for {:?} at {}",
            item_id,
            terms,
            timestamp
        );
        if let Some(item) = self.recent_workspaces.get(item_id) {
            event!(Level::INFO, item_id, "Launching recent item {:?}", item);
            self.launch_app_on_default_main_context(connection.clone(), Some(item.url.clone()))
                .await
        } else {
            event!(Level::ERROR, item_id, "Item not found");
            Err(zbus::fdo::Error::Failed(format!(
                "Result {item_id} not found"
            )))
        }
    }

    /// Launch a search within the App.
    ///
    /// This function is called when the user clicks on the provider icon to display more search results in the application.
    /// The arguments are the current search terms and a timestamp.
    ///
    /// Currently it simply launches the app without any arguments.
    #[instrument(skip(self, connection), fields(app_id = %self.app.id()))]
    async fn launch_search(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        _terms: Vec<String>,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        event!(Level::DEBUG, "Launching app directly");
        self.launch_app_on_default_main_context(connection.clone(), None)
            .await
    }
}
