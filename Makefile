DESTDIR =
PREFIX = /usr/local

## Files are installed into these three base directories.
# The path to install the service executable (gnome-search-providers-vscode)
BINDIR = $(PREFIX)/bin
# The path to install systemd user units in
USERUNITDIR = $(PREFIX)/lib/systemd/user
# The base path for dbus services and gnome-shell search providers
DATADIR = $(PREFIX)/share
DBUS_SERVICES_DIR = $(DATADIR)/dbus-1/services
SEARCH_PROVIDERS_DIR = $(DATADIR)/gnome-shell/search-providers

SEARCH_PROVIDERS = $(wildcard providers/*.ini)

.PHONY: build
build:
	cargo build --release --locked

.PHONY: install
install:
	install -Dm644 -t $(DESTDIR)$(SEARCH_PROVIDERS_DIR) $(SEARCH_PROVIDERS)
	install -Dm644 -t $(DESTDIR)$(USERUNITDIR) systemd/gnome-search-providers-vscode.service
	install -Dm644 -t $(DESTDIR)$(DBUS_SERVICES_DIR) dbus-1/de.swsnr.VSCodeSearchProvider.service
	install -Dm755 -t $(DESTDIR)$(BINDIR) target/release/gnome-search-providers-vscode
