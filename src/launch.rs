// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Launching apps.

use gio::prelude::*;
use glib::{Variant, VariantDict};
use tracing::{event, instrument, span, Level};
use tracing_futures::Instrument;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::systemd::{self, Systemd1ManagerProxy};

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

/**
 * Create a launch context.
 *
 * This context moves all launched applications to their own system scope.
 */
pub fn create_launch_context(connection: zbus::Connection) -> gio::AppLaunchContext {
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
}
