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
    // We must use Gtk's APIs to exit the app.
    clippy::exit,
    // Do not carelessly ignore errors
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
)]
#![allow(clippy::missing_panics_doc)]

use std::time::Duration;

use gio::prelude::*;
use gio::ApplicationFlags;
use glib::Object;

static G_LOG_DOMAIN: &str = "VSCodeSearchProvider";

mod workspaces {
    use std::path::Path;

    use gio::IOErrorEnum;
    use rusqlite::{OpenFlags, OptionalExtension};
    use serde::Deserialize;

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
                        "Failed to query recently opened path lists from VSCode global storage: {error}",
                    ),
                )
            })?
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    glib::Error::new(
                        IOErrorEnum::InvalidData,
                        &format!(
                            "Failed to deserialize recently opened path lists: {error}",
                        ),
                    )
                })
            })
            .transpose()
    }

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

    pub fn load_workspaces_from_path<P: AsRef<Path>>(
        db_path: P,
    ) -> Result<Vec<String>, glib::Error> {
        let connection = open_connection(db_path)?;
        load_workspaces(&connection)
    }
}

mod searchprovider2 {
    use std::borrow::Cow;

    use gio::{prelude::*, DBusNodeInfo, IOErrorEnum};
    use glib::{Variant, VariantDict};

    /// The literal XML definition of the interface.
    static SEARCH_PROVIDER2_XML: &str =
        include_str!("../dbus-1/org.gnome.ShellSearchProvider2.xml");

    #[derive(Debug, Variant)]
    pub struct GetInitialResultSet(pub Vec<String>);

    #[derive(Debug, Variant)]
    pub struct GetSubsearchResultSet(pub Vec<String>, pub Vec<String>);

    #[derive(Debug, Variant)]
    pub struct GetResultMetas(pub Vec<String>);

    #[derive(Debug, Variant)]
    pub struct ActivateResult(pub String, pub Vec<String>, pub u32);

    #[derive(Debug, Variant)]
    pub struct LaunchSearch(pub Vec<String>, pub u32);

    /// Method calls a search provider supports.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub enum Method {
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

    impl DBusMethodCall for Method {
        fn parse_call(
            _obj_path: &str,
            _interface: Option<&str>,
            method: &str,
            params: glib::Variant,
        ) -> Result<Self, glib::Error> {
            match method {
                "GetInitialResultSet" => params
                    .get::<GetInitialResultSet>()
                    .map(Method::GetInitialResultSet)
                    .ok_or_else(invalid_parameters),
                "GetSubsearchResultSet" => params
                    .get::<GetSubsearchResultSet>()
                    .map(Method::GetSubsearchResultSet)
                    .ok_or_else(invalid_parameters),
                "GetResultMetas" => params
                    .get::<GetResultMetas>()
                    .map(Method::GetResultMetas)
                    .ok_or_else(invalid_parameters),
                "ActivateResult" => params
                    .get::<ActivateResult>()
                    .map(Method::ActivateResult)
                    .ok_or_else(invalid_parameters),
                "LaunchSearch" => params
                    .get::<LaunchSearch>()
                    .map(Method::LaunchSearch)
                    .ok_or_else(invalid_parameters),
                _ => Err(glib::Error::new(
                    IOErrorEnum::InvalidArgument,
                    "Unexpected method",
                )),
            }
        }
    }

    #[derive(Debug, Default)]
    pub struct ResultMetas {
        pub id: String,
        pub name: String,
        pub description: String,
        pub icon: Option<Variant>,
    }

    impl ToVariant for ResultMetas {
        fn to_variant(&self) -> glib::Variant {
            let dict = VariantDict::new(None);
            dict.insert("id", &self.id);
            dict.insert("name", &self.name);
            dict.insert("description", &self.description);
            if let Some(icon) = &self.icon {
                dict.insert("icon", icon);
            }
            dict.into()
        }
    }

    impl From<ResultMetas> for Variant {
        fn from(value: ResultMetas) -> Self {
            value.to_variant()
        }
    }

    impl StaticVariantType for ResultMetas {
        fn static_variant_type() -> Cow<'static, glib::VariantTy> {
            VariantDict::static_variant_type()
        }
    }

    pub fn load_interface() -> Result<gio::DBusInterfaceInfo, glib::Error> {
        DBusNodeInfo::for_xml(SEARCH_PROVIDER2_XML)?
            .lookup_interface("org.gnome.Shell.SearchProvider2")
            .ok_or(glib::Error::new(
                IOErrorEnum::NotFound,
                "Interface org.gnome.Shell.SearchProvider2 not found",
            ))
    }
}

mod search {
    use std::fmt::Debug;

    use glib::UriFlags;

    use super::G_LOG_DOMAIN;

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
    pub fn name_and_description_of_uri(uri_or_path: &str) -> (String, String) {
        match glib::Uri::parse(uri_or_path, UriFlags::NONE) {
            Ok(parsed_uri) => {
                let name = name_from_uri(parsed_uri.path().as_str())
                    .unwrap_or(uri_or_path)
                    .to_owned();
                let description = match parsed_uri.scheme().as_str() {
                    "file:" if parsed_uri.host().is_none() => parsed_uri.path().into(),
                    _ => parsed_uri.to_string(),
                };
                (name, description)
            }
            Err(error) => {
                glib::warn!("Failed to parse {uri_or_path} as URI: {error}");
                let name = name_from_uri(uri_or_path)
                    .unwrap_or(uri_or_path)
                    .to_string();
                let description = uri_or_path.to_string();
                (name, description)
            }
        }
    }
}

glib::wrapper! {
    pub struct SearchProviderServiceApplication(ObjectSubclass<imp::SearchProviderServiceApplication>)
        @extends gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl Default for SearchProviderServiceApplication {
    fn default() -> Self {
        Object::builder()
            .property("application-id", "de.swsnr.VSCodeSearchProvider")
            .property("flags", ApplicationFlags::IS_SERVICE)
            // Exit one minute after release the app, i.e. in our case after finishing
            // the last DBus call.
            .property(
                "inactivity-timeout",
                u32::try_from(Duration::from_secs(60).as_millis()).unwrap(),
            )
            .build()
    }
}

mod imp {
    use std::cell::RefCell;
    use std::ffi::OsStr;

    use futures_util::future::join_all;
    use gio::prelude::*;
    use gio::subclass::prelude::*;
    use gio::IOErrorEnum;

    #[allow(clippy::wildcard_imports)]
    use super::searchprovider2::*;
    use super::{search, searchprovider2, G_LOG_DOMAIN};

    async fn get_icon(desktop_id: &'static str) -> Option<glib::Variant> {
        gio::spawn_blocking(|| {
            gio::DesktopAppInfo::new(desktop_id)
                .and_then(|app| app.icon())
                .and_then(|icon| icon.serialize())
        })
        .await
        .unwrap()
    }

    async fn get_result_metas(desktop_id: &'static str, uri: &str) -> ResultMetas {
        let (name, description) = search::name_and_description_of_uri(uri);
        ResultMetas {
            id: uri.to_string(),
            name,
            description,
            icon: get_icon(desktop_id).await,
        }
    }

    // Known providers, as pair of desktop ID and configuration directory.
    static PROVIDERS: [(&str, &str); 3] = [
        // The standard Arch Linux code package from community
        ("code-oss.desktop", "Code - OSS"),
        // The standard codium package on Linux from here: https://github.com/VSCodium/vscodium.
        // Should work for most Linux distributions packaged from here.
        ("codium.desktop", "VSCodium"),
        // The official install packages from https://code.visualstudio.com/download
        ("code.desktop", "Code"),
    ];

    #[derive(Default)]
    pub struct SearchProviderServiceApplication {
        registered_object: RefCell<Vec<gio::RegistrationId>>,
    }

    impl SearchProviderServiceApplication {
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
        /// scope first with systemd-run and then spawn the app in with gio launch.
        async fn launch_uri(
            &self,
            desktop_id: &'static str,
            uri: Option<&str>,
        ) -> Result<(), glib::Error> {
            let desktop_file = gio::spawn_blocking(|| {
                gio::DesktopAppInfo::new(desktop_id).and_then(|app| app.filename())
            })
            .await
            .unwrap()
            .ok_or(glib::Error::new(
                IOErrorEnum::NotFound,
                &format!("Application {desktop_id} not found"),
            ))?;
            let mut command = vec![
                OsStr::new("/usr/bin/systemd-run"),
                OsStr::new("--user"),
                OsStr::new("--scope"),
                OsStr::new("--same-dir"),
                OsStr::new("/usr/bin/gio"),
                OsStr::new("launch"),
                OsStr::new(&desktop_file),
            ];
            command.extend_from_slice(uri.map(OsStr::new).as_slice());
            glib::info!("Launching command {:?}", command);
            let process = gio::Subprocess::newv(command.as_slice(), gio::SubprocessFlags::NONE)?;
            process.wait_future().await?;
            glib::info!("Command {:?} finished", command);
            Ok(())
        }

        async fn dispatch_search_provider(
            &self,
            call: searchprovider2::Method,
            desktop_id: &'static str,
            config_directory: &str,
        ) -> Result<Option<glib::Variant>, glib::Error> {
            let _guard = self.obj().hold();
            match call {
                Method::GetInitialResultSet(GetInitialResultSet(terms)) => {
                    glib::debug!("Searching for terms {terms:?}");
                    let db_path = glib::user_config_dir()
                        .join(config_directory)
                        .join("User")
                        .join("globalStorage")
                        .join("state.vscdb");
                    glib::info!("Loading workspaces from db at {}", db_path.display());
                    let workspaces = gio::spawn_blocking(move || {
                        glib::debug!("Loading workspaces from {}", db_path.display());
                        super::workspaces::load_workspaces_from_path(db_path)
                    })
                    .await
                    .unwrap()?;

                    Ok(Some(
                        search::find_matching_uris(&workspaces, terms.as_slice()).into(),
                    ))
                }
                Method::GetSubsearchResultSet(GetSubsearchResultSet(previous_results, terms)) => {
                    glib::debug!(
                        "Searching for terms {terms:?} in {} previous results",
                        previous_results.len()
                    );
                    Ok(Some(
                        search::find_matching_uris(previous_results, terms.as_slice()).into(),
                    ))
                }
                Method::GetResultMetas(GetResultMetas(identifiers)) => {
                    let metas = join_all(
                        identifiers
                            .iter()
                            .map(|uri| get_result_metas(desktop_id, uri)),
                    )
                    .await;
                    Ok(Some(metas.into()))
                }
                Method::ActivateResult(ActivateResult(identifier, _, _)) => {
                    glib::info!("Launching application {desktop_id} with URI {identifier}",);
                    self.launch_uri(desktop_id, Some(identifier.as_ref()))
                        .await?;
                    Ok(None)
                }
                Method::LaunchSearch(_) => {
                    glib::info!("Launching application {desktop_id} directly",);
                    self.launch_uri(desktop_id, None).await?;
                    Ok(None)
                }
            }
        }

        fn register_all_providers(
            &self,
            connection: &gio::DBusConnection,
            base_path: &str,
        ) -> Result<(), glib::Error> {
            let interface = searchprovider2::load_interface()?;
            for (desktop_id, config_directory) in PROVIDERS {
                let object_path = format!(
                    "{base_path}/{}",
                    desktop_id.replace('-', "_").trim_end_matches(".desktop")
                );
                glib::debug!("Registering provider for {desktop_id} at {object_path}");
                let id = connection
                    .register_object(&object_path, &interface)
                    .typed_method_call::<searchprovider2::Method>()
                    .invoke_and_return_future_local(glib::clone!(
                        #[strong(rename_to = app)]
                        self.obj(),
                        move |_, _, call| {
                            let _guard = app.hold();
                            let app = app.clone();
                            async move {
                                app.imp()
                                    .dispatch_search_provider(call, desktop_id, config_directory)
                                    .await
                            }
                        }
                    ))
                    .build()?;
                self.registered_object.borrow_mut().push(id);
            }
            Ok(())
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SearchProviderServiceApplication {
        const NAME: &'static str = "SearchProviderServiceApplication";

        type Type = super::SearchProviderServiceApplication;

        type ParentType = gio::Application;
    }

    impl ObjectImpl for SearchProviderServiceApplication {}

    impl ApplicationImpl for SearchProviderServiceApplication {
        fn dbus_register(
            &self,
            connection: &gio::DBusConnection,
            object_path: &str,
        ) -> Result<bool, glib::Error> {
            self.parent_dbus_register(connection, object_path)?;
            self.register_all_providers(connection, object_path)?;
            Ok(true)
        }

        fn dbus_unregister(&self, connection: &gio::DBusConnection, object_path: &str) {
            self.parent_dbus_unregister(connection, object_path);
            for id in self.registered_object.take() {
                if let Err(error) = connection.unregister_object(id) {
                    glib::warn!("Failed to unregister object: {error}");
                }
            }
        }
    }
}

fn main() -> glib::ExitCode {
    static LOGGER: glib::GlibLogger = glib::GlibLogger::new(
        glib::GlibLoggerFormat::Structured,
        glib::GlibLoggerDomain::CrateTarget,
    );
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    let app = SearchProviderServiceApplication::default();
    app.set_version(env!("CARGO_PKG_VERSION"));
    app.run()
}
