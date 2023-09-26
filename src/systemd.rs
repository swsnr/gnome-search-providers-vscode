// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Systemd utilities.

use zbus::dbus_proxy;
use zbus::zvariant::{OwnedObjectPath, Value};

/// The systemd manager DBUS API.
///
/// See <https://www.freedesktop.org/wiki/Software/systemd/dbus/>
#[dbus_proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Systemd1Manager {
    /// StartTransientUnit() may be used to create and start a transient unit,
    /// which will be released as soon as it is not running or referenced anymore or the system is rebooted.
    ///
    /// `name` is the unit name including suffix, and must be unique. `mode` is the same as in `StartUnit()`,
    /// `properties` contains properties of the unit, specified like in `SetUnitProperties()`.
    /// `aux` is currently unused and should be passed as empty array.
    fn start_transient_unit(
        &self,
        name: &str,
        mode: &str,
        properties: &[(&str, Value<'_>)],
        aux: &[(&str, Vec<(&str, Value<'_>)>)],
    ) -> zbus::Result<OwnedObjectPath>;
}

/// Properties for a new systemd scope.
#[derive(Debug)]
pub struct ScopeProperties<'a> {
    /// The prefix to prepend before the scope name.
    ///
    /// This string is prepended **literally**, and not escaped.
    ///
    /// It must be a valid string for a systemd unit.
    pub prefix: &'a str,
    /// The name for the scope.
    ///
    /// This string gets escaped for systemd and can be arbitrary.
    pub name: &'a str,
    /// The optional description for the unit.
    ///
    /// This is used by systemd (and other UIs) as the label for the unit,
    /// so this string should identify the unit rather than describe it, despite the name.
    ///
    /// See `systemd.unit(5)` for more information.
    pub description: Option<&'a str>,
    /// The optional documentation URLs for the unit.
    pub documentation: Vec<&'a str>,
}

/// Escape a systemd unit name.
///
/// See section "STRING ESCAPING FOR INCLUSION IN UNIT NAMES" in `systemd.unit(5)`
/// for details about the algorithm.
pub fn escape_name(name: &str) -> String {
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
