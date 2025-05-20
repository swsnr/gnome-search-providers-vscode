// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// Licensed under the EUPL
//
// See https://interoperable-europe.ec.europa.eu/collection/eupl/eupl-text-eupl-12

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
        Some(dirs) => std::env::split_paths(&dirs).collect(),
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
        let data_dirs = data_dirs();
        std::iter::once(&data_home())
            .chain(&data_dirs)
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
