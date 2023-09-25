// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#[derive(Debug, Copy, Clone)]
pub struct ConfigLocation<'a> {
    pub dirname: &'a str,
}

/// A search provider to expose from this service.
pub struct ProviderDefinition<'a> {
    /// A human readable label for this provider.
    pub label: &'a str,
    /// The ID (that is, the filename) of the desktop file of the corresponding app.
    pub desktop_id: &'a str,
    /// The relative object path to expose this provider at.
    pub relative_obj_path: &'a str,
    /// The location of the configuration for this app.
    pub config: ConfigLocation<'a>,
}

impl ProviderDefinition<'_> {
    /// Gets the full object path for this provider.
    pub fn objpath(&self) -> String {
        format!("/de/swsnr/searchprovider/vscode/{}", self.relative_obj_path)
    }
}

/// Known search providers.
///
/// For each definition in this array a corresponding provider file must exist in
/// `providers/`; the file must refer to the same `desktop_id` and the same object path.
/// The object path must be unique for each desktop ID, to ensure that this service always
/// launches the right application associated with the search provider.
pub const PROVIDERS: &[ProviderDefinition] = &[
    // The standard Arch Linux code package from community
    ProviderDefinition {
        label: "Code OSS (Arch Linux)",
        desktop_id: "code-oss.desktop",
        relative_obj_path: "arch/codeoss",
        config: ConfigLocation {
            dirname: "Code - OSS",
        },
    },
    // The binary AUR package for visual studio code: https://aur.archlinux.org/packages/visual-studio-code-bin/
    ProviderDefinition {
        label: "Visual Studio Code (AUR package)",
        desktop_id: "visual-studio-code.desktop",
        relative_obj_path: "aur/visualstudiocode",
        config: ConfigLocation { dirname: "Code" },
    },
    // The standard codium package on Linux from here: https://github.com/VSCodium/vscodium.
    // Should work for most Linux distributions packaged from here.
    ProviderDefinition {
        label: "VSCodium",
        desktop_id: "codium.desktop",
        relative_obj_path: "codium",
        config: ConfigLocation {
            dirname: "VSCodium",
        },
    },
    // The official install packages from https://code.visualstudio.com/download.
    ProviderDefinition {
        label: "Visual Studio Code (Official package)",
        desktop_id: "code.desktop",
        relative_obj_path: "official/code",
        config: ConfigLocation { dirname: "Code" },
    },
];

#[cfg(test)]
mod tests {
    use crate::{BUSNAME, PROVIDERS};
    use anyhow::{anyhow, Context, Result};
    use std::collections::HashSet;
    use std::path::Path;

    struct ProviderFile {
        desktop_id: String,
        object_path: String,
        bus_name: String,
        version: String,
    }

    fn load_all_provider_files() -> Result<Vec<ProviderFile>> {
        let mut providers = Vec::new();
        let ini_files = globwalk::GlobWalkerBuilder::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("providers"),
            "*.ini",
        )
        .build()
        .unwrap();
        for entry in ini_files {
            let filepath = entry.unwrap().into_path();
            let mut ini = configparser::ini::Ini::new();
            ini.load(&filepath).map_err(|s| {
                anyhow!("Failed to parse ini file at {}: {}", filepath.display(), s)
            })?;
            let provider = ProviderFile {
                desktop_id: ini
                    .get("Shell Search Provider", "DesktopId")
                    .with_context(|| format!("DesktopId missing in {}", &filepath.display()))?,
                object_path: ini
                    .get("Shell Search Provider", "ObjectPath")
                    .with_context(|| format!("ObjectPath missing in {}", &filepath.display()))?,
                bus_name: ini
                    .get("Shell Search Provider", "BusName")
                    .with_context(|| format!("BusName missing in {}", &filepath.display()))?,
                version: ini
                    .get("Shell Search Provider", "Version")
                    .with_context(|| format!("Version missing in {}", &filepath.display()))?,
            };
            providers.push(provider);
        }

        Ok(providers)
    }

    #[test]
    fn all_providers_have_a_correct_ini_file() {
        let provider_files = load_all_provider_files().unwrap();
        for provider in PROVIDERS {
            let provider_file = provider_files
                .iter()
                .find(|p| p.desktop_id == provider.desktop_id);
            assert!(
                provider_file.is_some(),
                "Provider INI missing for provider {} with desktop ID {}",
                provider.label,
                provider.desktop_id
            );

            assert_eq!(provider_file.unwrap().object_path, provider.objpath());
            assert_eq!(provider_file.unwrap().bus_name, BUSNAME);
            assert_eq!(provider_file.unwrap().version, "2");
        }
    }

    #[test]
    fn no_extra_ini_files_without_providers() {
        let provider_files = load_all_provider_files().unwrap();
        assert_eq!(PROVIDERS.len(), provider_files.len());
    }

    #[test]
    fn desktop_ids_are_unique() {
        let mut ids = HashSet::new();
        for provider in PROVIDERS {
            ids.insert(provider.desktop_id);
        }
        assert_eq!(PROVIDERS.len(), ids.len());
    }

    #[test]
    fn dbus_paths_are_unique() {
        let mut paths = HashSet::new();
        for provider in PROVIDERS {
            paths.insert(provider.objpath());
        }
        assert_eq!(PROVIDERS.len(), paths.len());
    }
}
