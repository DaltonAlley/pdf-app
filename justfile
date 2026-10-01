default:
    @just --list

dev:
    nu --no-config-file scripts/dev.nu

check:
    scripts/check.nu pdf-app

fmt:
    cargo fmt --all
    cargo fmt --manifest-path frontend/Cargo.toml

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo clippy --manifest-path frontend/Cargo.toml --all-targets --locked -- -D warnings
    cargo clippy --manifest-path frontend/Cargo.toml --target wasm32-unknown-unknown --release --locked -- -D warnings

test:
    cargo nextest run --workspace --all-features
    cargo nextest run --manifest-path frontend/Cargo.toml --locked

build:
    cargo build --locked
    cd frontend && cargo leptos build --frontend-only --split --lib-cargo-args=--locked

release:
    cargo build --release --locked
    nu --no-config-file frontend/scripts/build-release.nu

clean:
    cargo clean
    cargo clean --manifest-path frontend/Cargo.toml
    rm -rf frontend/dist
