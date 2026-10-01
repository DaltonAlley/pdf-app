#!/usr/bin/env nu

const project_root = path self | path dirname | path join .. | path expand

def run-command [project_dir: path command: closure] {
    let result = do { cd $project_dir; do $command } | complete
    if not ($result.stdout | is-empty) { print --no-newline $result.stdout }
    if not ($result.stderr | is-empty) { print --stderr --no-newline $result.stderr }
    $result.exit_code
}

def check-pdf-app [project_dir: path] {
    for executable in [cargo cargo-leptos cargo-nextest rustup gzip curl] {
        if (which $executable | is-empty) {
            print --stderr $"Missing required command: ($executable)"
            return 127
        }
    }
    let leptos_version = do { ^cargo leptos --version } | complete
    if $leptos_version.exit_code != 0 or ($leptos_version.stdout | str trim) != "cargo-leptos 0.3.7" {
        return 1
    }
    let nextest_version = do { ^cargo nextest --version } | complete
    if $nextest_version.exit_code != 0 or not (($nextest_version.stdout | lines | first) | str starts-with "cargo-nextest 0.9.100") {
        return 1
    }
    let installed_targets = do { ^rustup target list --installed } | complete
    if $installed_targets.exit_code != 0 or "wasm32-unknown-unknown" not-in ($installed_targets.stdout | lines) {
        return 1
    }
    let commands = [
        {|| ^cargo fmt --all --check }
        {|| ^cargo clippy --locked --workspace --all-targets --all-features -- -D warnings }
        {|| ^cargo nextest run --locked --workspace --all-features }
        {|| ^cargo doc --locked --workspace --all-features --no-deps }
        {|| ^cargo fmt --manifest-path frontend/Cargo.toml --check }
        {|| ^cargo clippy --manifest-path frontend/Cargo.toml --all-targets --locked -- -D warnings }
        {|| ^cargo nextest run --manifest-path frontend/Cargo.toml --locked }
        {|| ^cargo clippy --manifest-path frontend/Cargo.toml --target wasm32-unknown-unknown --release --locked -- -D warnings }
        {|| ^cargo check --manifest-path frontend/Cargo.toml --target wasm32-unknown-unknown --release --locked }
    ]
    for command in $commands {
        let status = run-command $project_dir $command
        if $status != 0 { return $status }
    }
    let build_status = with-env { NO_COLOR: "true" } {
        run-command ($project_dir | path join frontend) { ^nu --no-config-file scripts/build-release.nu }
    }
    if $build_status != 0 { return $build_status }
    for artifact in [dist/app.html dist/asset-hashes.txt] {
        if not ($project_dir | path join frontend $artifact | path exists) { return 1 }
    }
    run-command $project_dir { ^nu --no-config-file scripts/runtime-smoke.nu }
}

def main [project: string = "pdf-app"] {
    if $project != "pdf-app" {
        print --stderr $"Unknown project: ($project)"
        exit 2
    }
    let status = check-pdf-app $project_root
    if $status != 0 { exit $status }
    print "All requested checks passed."
}
