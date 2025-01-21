// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::{
    io::{Error, ErrorKind, Result},
    path::{Path, PathBuf},
};

use configparser::ini::Ini;

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

#[derive(Debug)]
pub struct DesktopEntry {
    path: PathBuf,
    icon: Option<String>,
}

impl DesktopEntry {
    fn from_path(path: PathBuf) -> Result<DesktopEntry> {
        let mut config = Ini::new();
        config
            .load(&path)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        let icon = config.get("Desktop Entry", "icon");
        Ok(DesktopEntry { path, icon })
    }

    pub fn find(app_id: &str) -> Option<DesktopEntry> {
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
            .find_map(|file| DesktopEntry::from_path(file).ok())
    }

    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
