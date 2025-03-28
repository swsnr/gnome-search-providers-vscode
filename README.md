# Gnome search provider for VSCode workspaces

Add recent workspaces of various VSCode variants to Gnome search.

- Code OSS (Arch Linux)
- VSCodium
- Office Visual Studio Code packages
- Visual Studio Code Insiders

Under the hood this is a small systemd user service which implements the [search provider][1] DBus API and exposes recent workspaces from VSCode.

[1]: https://developer.gnome.org/SearchProvider/documentation/tutorials/search-provider.html

## Installation

### Packages & binaries

I provide a binary package at [home:swsnr](https://build.opensuse.org/repositories/home:swsnr).

### From source

Install [rust](https://www.rust-lang.org/tools/install) then run

```console
$ make build
$ sudo make install
```

This install to `/usr/local/`.

**Note:** You really do need to install as `root`, system-wide.
A per-user installation to `$HOME` does not work as of Gnome 40, because Gnome shell doesn't load search providers from `$HOME` (see <https://gitlab.gnome.org/GNOME/gnome-shell/-/issues/3060>).

## License

Copyright Sebastian Wiesner <sebastian@swsnr.de>

This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at <http://mozilla.org/MPL/2.0/>.
