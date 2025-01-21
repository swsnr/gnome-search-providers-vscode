// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

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
            serde_json::from_value(value).map_err(|error| Error::new(ErrorKind::InvalidData, error))
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
