// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use anyhow::{Context, Result};
use rusqlite::{OpenFlags, OptionalExtension};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tracing::{event, instrument, Level};

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
pub struct StorageOpenedPathsList {
    entries: Option<Vec<StorageOpenedPathsListEntry>>,
}

impl StorageOpenedPathsList {
    pub fn into_workspace_urls(self) -> Vec<String> {
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
pub struct GlobalStorage {
    connection: rusqlite::Connection,
}

impl GlobalStorage {
    /// Open a global storage database at the given path.
    pub fn open_file<P: AsRef<Path>>(file: P) -> rusqlite::Result<Self> {
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
    pub fn database_path_in_config_dir<P: AsRef<Path>>(directory: P) -> PathBuf {
        directory
            .as_ref()
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }

    // Open the global storage database in the given configuration `directory`.
    pub fn open_from_config_directory<P: AsRef<Path>>(directory: P) -> rusqlite::Result<Self> {
        Self::open_file(Self::database_path_in_config_dir(directory))
    }

    /// Query recently opened path lists.
    #[instrument(skip(self))]
    pub fn recently_opened_paths_list(&self) -> Result<StorageOpenedPathsList> {
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

#[cfg(test)]
mod tests {
    use crate::providers::PROVIDERS;
    use crate::storage::GlobalStorage;
    use gnome_search_provider_common::glib;

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
}
