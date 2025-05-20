destdir := ''
prefix := '/usr/local'

bindir := prefix / 'bin'
userunitdir := prefix / 'lib/systemd/user'
datadir := prefix / 'share'
dbus_services_dir := datadir / 'dbus-1/services'
search_providers_dir := datadir / 'gnome-shell/search-providers'

default:
    just --list

clean:
    rm -rf dist vendor

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

_dist:
    rm -rf dist
    mkdir -p dist

# Build and sign a reproducible archive of cargo vendor sources
_vendor: _dist
    rm -rf vendor/
    cargo vendor --locked
    echo SOURCE_DATE_EPOCH="$(env LC_ALL=C TZ=UTC0 git show --quiet --date='format-local:%Y-%m-%dT%H:%M:%SZ' --format="%cd" HEAD)"
    # See https://reproducible-builds.org/docs/archives/
    env LC_ALL=C TZ=UTC0 tar --numeric-owner --owner 0 --group 0 \
        --sort name --mode='go+u,go-w' --format=posix \
        --pax-option=exthdr.name=%d/PaxHeaders/%f \
        --pax-option=delete=atime,delete=ctime \
        --mtime="$(env LC_ALL=C TZ=UTC0 git show --quiet --date='format-local:%Y-%m-%dT%H:%M:%SZ' --format="%cd" HEAD)" \
        -c -f "dist/gnome-search-providers-vscode-$(git describe)-vendor.tar.zst" \
        --zstd vendor

# Build and sign a reproducible git archive bundle
_git-archive: _dist
    env LC_ALL=C TZ=UTC0 git archive --format tar \
        --prefix "gnome-search-providers-vscode-$(git describe)/" \
        --output "dist/gnome-search-providers-vscode-$(git describe).tar" HEAD
    zstd --rm "dist/gnome-search-providers-vscode-$(git describe).tar"

package: _git-archive _vendor
    curl https://codeberg.org/swsnr.keys > dist/key
    ssh-keygen -Y sign -f dist/key -n file "dist/gnome-search-providers-vscode-$(git describe).tar.zst"
    ssh-keygen -Y sign -f dist/key -n file "dist/gnome-search-providers-vscode-$(git describe)-vendor.tar.zst"
    rm dist/key

_post-release:
    @echo "Create a release for the new version at https://codeberg.org/swsnr/gnome-search-providers-vscode/tags"
    @echo "Upload dist/ to the release"

release *ARGS: test-all && package _post-release
    cargo release {{ARGS}}
