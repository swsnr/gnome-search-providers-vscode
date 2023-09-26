// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Reload all items of all providers.

use crate::providers::PROVIDERS;
use crate::searchprovider::VSCodeWorkspaceSearchProvider;
use tracing::{event, instrument, Level};
use zbus::{dbus_interface, ObjectServer};

#[derive(Debug)]
pub struct ReloadAll;

#[dbus_interface(name = "de.swsnr.searchprovider.ReloadAll")]
impl ReloadAll {
    /// Refresh all items in the search provider.
    #[instrument(skip(self, server))]
    pub async fn reload_all(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<()> {
        event!(Level::DEBUG, "Reloading all search provider items");
        let mut is_failed = false;
        for provider in PROVIDERS {
            match server
                .interface::<_, VSCodeWorkspaceSearchProvider>(provider.objpath())
                .await
            {
                Err(error) => {
                    event!(
                        Level::DEBUG,
                        "Skipping {} ({}): {error}",
                        provider.label,
                        provider.desktop_id
                    );
                }
                Ok(search_provider_interface) => {
                    if let Err(error) = search_provider_interface
                        .get_mut()
                        .await
                        .reload_recent_workspaces()
                    {
                        is_failed = true;
                        let iface = search_provider_interface.get().await;
                        let app_id = iface.app().id();
                        event!(Level::ERROR, %app_id, "Failed to refresh items of {}: {}", app_id, error);
                    }
                }
            }
        }
        if is_failed {
            Err(zbus::fdo::Error::Failed(
                "Failed to reload some items".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}
