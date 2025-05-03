default:
    just --list

# Vet only Linux dependencies.
vet *ARGS:
    @# CARGO_BUILD_TARGET for this Seems to be unofficial, see https://github.com/mozilla/cargo-vet/issues/579, but works
    env CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu cargo +stable vet {{ARGS}}

test-all:
    just vet --locked
    cargo +stable deny --all-features --locked check
    cargo +stable fmt -- --check
    cargo +stable build --all-targets --locked
    cargo +stable clippy --all-targets --locked
    cargo +stable test --locked
