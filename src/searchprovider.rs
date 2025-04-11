// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::{
    io::{Error, ErrorKind},
    path::PathBuf,
};

use futures_util::future::join_all;
use serde::Serialize;
use tokio::{process::Command, sync::OnceCell};
use tracing::{Span, debug, info, instrument};
use url::Url;
use zbus::{
    interface,
    zvariant::{Array, OwnedValue, SerializeDict, Str, Type},
};

use super::{search, workspaces, xdg};

#[derive(Debug, Type, Serialize)]
#[zvariant(signature = "(sv)")]
struct SerializedIcon(&'static str, OwnedValue);

impl SerializedIcon {
    fn from_desktop_entry(entry: &xdg::DesktopEntry) -> Option<Self> {
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
    desktop_entry: OnceCell<Option<xdg::DesktopEntry>>,
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
        let scope_name = format!(
            "app-gnome-{}-{}.scope",
            self.code.app_id.replace('-', "_"),
            fastrand::u16(..)
        );
        info!("Launching {} in new scope {}", self.code.app_id, scope_name);
        Command::new("/usr/bin/systemd-run")
            .arg("--unit")
            .arg(&scope_name)
            .args(["--user", "--scope", "--same-dir", "/usr/bin/gio", "launch"])
            .arg(desktop_entry.path().as_os_str())
            .args(uri.as_slice())
            .spawn()?;
        Ok(())
    }

    async fn desktop_entry(&self) -> Option<&xdg::DesktopEntry> {
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
            .and_then(SerializedIcon::from_desktop_entry)
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

#[derive(Copy, Clone)]
pub struct CodeVariant {
    pub app_id: &'static str,
    pub config_directory_name: &'static str,
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
    fn find_desktop_entry(&self) -> Option<xdg::DesktopEntry> {
        xdg::DesktopEntry::find(self.app_id).inspect(|desktop_entry| {
            debug!(
                "Found desktop entry {} for {}",
                desktop_entry.path().display(),
                self.app_id,
            );
        })
    }
}
