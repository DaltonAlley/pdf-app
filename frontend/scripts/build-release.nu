#!/usr/bin/env nu

const frontend_dir = path self ..

# Build a mutually versioned bootstrap and deferred modules, then resolve the
# static CSR entrypoint from cargo-leptos's authoritative asset hashes.
def main [] {
    cd $frontend_dir
    let site_root = $env.LEPTOS_SITE_ROOT? | default ($frontend_dir | path join dist) | path expand
    let hash_file = $site_root | path join asset-hashes.txt
    let package_dir = $site_root | path join pkg
    mkdir $site_root
    # Do not include stale sidecars or an earlier version directory in the hash.
    if ($package_dir | path exists) { rm --recursive $package_dir }
    with-env {
        LEPTOS_SITE_ROOT: $site_root
        LEPTOS_HASH_FILES: "true"
        LEPTOS_HASH_FILE_NAME: $hash_file
        NO_COLOR: "true"
    } {
        ^cargo leptos build --frontend-only --split --release --lib-cargo-args=--locked
        if $env.LAST_EXIT_CODE != 0 {
            error make { msg: "split frontend release build failed" }
        }
    }

    # Namespace the complete linked bundle as well. A loader's embedded module
    # references may change even when its own pre-rewrite hash stays the same.
    let bundle_hash = glob ($package_dir | path join "**/*")
        | where {|path| ($path | path type) == file }
        | sort
        | each {|path| ($path | path relative-to $package_dir) + ":" + (open --raw $path | hash sha256) }
        | str join "\n"
        | hash sha256
    let entries = ls $package_dir | get name
    let versioned_dir = $package_dir | path join $bundle_hash
    mkdir $versioned_dir
    for entry in $entries { mv $entry $versioned_dir }

    let hashes = open --raw $hash_file | lines | parse "{kind}: {hash}"
    mut html = open --raw ($frontend_dir | path join assets app.html)
    for extension in [js wasm css] {
        let matches = $hashes | where kind == $extension
        if ($matches | length) != 1 or ($matches.0.hash !~ '^[A-Za-z0-9_-]+$') {
            error make { msg: $"missing or invalid cargo-leptos hash for ($extension)" }
        }
        let name = $"pdf-tools-frontend.($matches.0.hash).($extension)"
        if not ($versioned_dir | path join $name | path exists) {
            error make { msg: $"missing release asset: ($name)" }
        }
        $html = $html | str replace --all $"/pkg/pdf-tools-frontend.($extension)" $"/pkg/($bundle_hash)/($name)"
    }
    $html | save --force ($site_root | path join app.html)
    ^nu --no-config-file ($frontend_dir | path join scripts compress-assets.nu) $site_root
    if $env.LAST_EXIT_CODE != 0 {
        error make { msg: "frontend asset compression failed" }
    }
}
