destdir := ''
prefix := '/usr/local'

bindir := prefix / 'bin'
userunitdir := prefix / 'lib/systemd/user'
datadir := prefix / 'share'
dbus_services_dir := datadir / 'dbus-1/services'
search_providers_dir := datadir / 'gnome-shell/search-providers'

default:
    just --list

install:
	install -Dm644 -t {{destdir}}/{{search_providers_dir}} providers/*.ini
	install -Dm644 -t {{destdir}}/{{userunitdir}} systemd/gnome-search-providers-vscode.service
	install -Dm644 -t {{destdir}}/{{dbus_services_dir}} dbus-1/de.swsnr.VSCodeSearchProvider.service
	install -Dm755 -t {{destdir}}/{{bindir}} target/release/gnome-search-providers-vscode

vet *ARGS:
    cargo +stable vet {{ARGS}}

test-all:
    just vet --locked
    cargo +stable deny --all-features --locked check
    cargo +stable fmt -- --check
    cargo +stable build --all-targets --locked
    cargo +stable clippy --all-targets --locked
    cargo +stable test --locked

release *ARGS: test-all
    cargo release {{ARGS}}
